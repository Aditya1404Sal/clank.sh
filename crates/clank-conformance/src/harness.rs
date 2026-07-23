//! Scenario discovery + libtest-mimic wiring shared by the two test binaries.

use crate::backend::{golem::GolemBackend, native::NativeBackend, BackendKind, ShellBackend};
use crate::matcher;
use crate::scenario::{Action, Scenario};
use libtest_mimic::{Arguments, Failed, Trial};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Entry point for `tests/native.rs` / `tests/golem.rs`.
pub fn main(kind: BackendKind) -> ! {
    let mut args = Arguments::from_args();
    match kind {
        BackendKind::Native => {
            // Non-negotiable: the native Session leans on process-global cwd/fd/env state,
            // and `run_uu`'s fd swap is unsafe against ANY concurrently-printing thread —
            // including the test reporter itself.
            if args.test_threads.is_some_and(|n| n != 1) {
                eprintln!("note: native conformance always runs at --test-threads=1 (process-global shell state)");
            }
            args.test_threads = Some(1);
        }
        BackendKind::Golem => {
            // Scenario instances are independent, but default conservative; the wrapper
            // script raises this explicitly once concurrency is proven on a server.
            if args.test_threads.is_none() {
                args.test_threads = Some(1);
            }
        }
    }

    let golem_enabled = std::env::var("CLANK_CONFORMANCE_GOLEM").is_ok_and(|v| v == "1");
    if kind == BackendKind::Golem && golem_enabled {
        if let Err(msg) = preflight() {
            eprintln!("golem conformance preflight failed: {msg}");
            std::process::exit(1);
        }
    }

    let trials = build_trials(kind, golem_enabled);
    libtest_mimic::run(&args, trials).exit()
}

fn scenarios_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("scenarios")
}

fn build_trials(kind: BackendKind, golem_enabled: bool) -> Vec<Trial> {
    let dir = scenarios_dir();
    let mut paths: Vec<PathBuf> = match std::fs::read_dir(&dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "clank"))
            .collect(),
        Err(e) => {
            return vec![Trial::test("scenario-discovery", move || {
                Err(Failed::from(format!("cannot read {}: {e}", dir.display())))
            })];
        }
    };
    paths.sort();

    let mut trials = Vec::with_capacity(paths.len());
    for path in paths {
        let stem = path
            .file_stem().map_or_else(|| path.display().to_string(), |s| s.to_string_lossy().into_owned());

        // The stem becomes the golem agent id and is interpolated into the injected
        // `mkdir -p` step — enforce the safe charset at the source.
        if stem.is_empty() || !stem.chars().all(|c| matches!(c, 'a'..='z' | '0'..='9' | '-')) {
            let msg = format!(
                "scenario file name `{stem}` must be non-empty [a-z0-9-] (it becomes the agent id and sandbox path)"
            );
            trials.push(Trial::test(stem, move || Err(Failed::from(msg))));
            continue;
        }

        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                let msg = format!("cannot read {}: {e}", path.display());
                trials.push(Trial::test(stem, move || Err(Failed::from(msg))));
                continue;
            }
        };
        let scenario = match crate::scenario::parse(&stem, &path, &text) {
            Ok(s) => s,
            Err(e) => {
                // A malformed file is a loud failure, never a silent skip.
                let msg = e.to_string();
                trials.push(Trial::test(stem, move || Err(Failed::from(msg))));
                continue;
            }
        };

        // Not-runnable trials are reported `ignored`; if someone forces them anyway
        // (`--ignored`/`--include-ignored`), they fail fast with the reason instead of
        // running a scenario that cannot validly run on this tier.
        let not_runnable: Option<String> = if kind == BackendKind::Golem && !golem_enabled {
            Some("golem tier disabled — run scripts/conformance-golem.sh (or set CLANK_CONFORMANCE_GOLEM=1 against a running server)".into())
        } else if scenario.only.is_some_and(|only| only != kind) {
            Some(format!("scenario is @only {} — not runnable on the {} tier", scenario.only.unwrap().label(), kind.label()))
        } else if !scenario.requires.is_empty() {
            // Tier-gated scenarios (`@requires network|llm|grease|mcp`) stay visible-but-
            // ignored until those tiers are wired into the harness.
            Some(format!("requires tier(s) {:?} — not wired into the harness yet", scenario.requires))
        } else {
            None
        };

        let ignored = not_runnable.is_some();
        trials.push(
            Trial::test(stem, move || match not_runnable {
                Some(reason) => Err(Failed::from(reason)),
                None => run_scenario(kind, scenario),
            })
            .with_ignored_flag(ignored),
        );
    }
    trials
}

// scenario is owned by the trial closure and consumed here — the terminal consumer; by-ref would just push the move elsewhere
#[allow(clippy::needless_pass_by_value)]
fn run_scenario(kind: BackendKind, scenario: Scenario) -> Result<(), Failed> {
    let infra = |line: usize, e: anyhow::Error| {
        Failed::from(format!(
            "{}:{line}: infrastructure failure (not an assertion mismatch): {e:#}",
            scenario.path.display()
        ))
    };

    let mut backend: Box<dyn ShellBackend> = match kind {
        BackendKind::Native => Box::new(NativeBackend::new(&scenario.name).map_err(|e| infra(0, e))?),
        BackendKind::Golem => Box::new(GolemBackend::new(&scenario.name).map_err(|e| infra(0, e))?),
    };

    // Teardown must run whether the steps passed or not — a failing trial that skips
    // `finish()` leaks its env mutations and sandbox into every later trial in this
    // process. So: run the steps, THEN finish, then combine the outcomes.
    let steps_result = run_steps(kind, &scenario, backend.as_mut());
    let teardown = backend
        .finish()
        .map_err(|e| format!("{}: teardown failed: {e:#}", scenario.path.display()));

    match (steps_result, teardown) {
        (Err(step_failure), Err(teardown_failure)) => {
            Err(Failed::from(format!("{step_failure}\n\n(additionally: {teardown_failure})")))
        }
        (Err(step_failure), Ok(())) => Err(Failed::from(step_failure)),
        (Ok(()), Err(teardown_failure)) => Err(Failed::from(teardown_failure)),
        (Ok(()), Ok(())) => Ok(()),
    }
}

/// Execute the injected sandbox step plus every scenario step; returns the rendered
/// failure text on the first mismatch or infrastructure error.
fn run_steps(kind: BackendKind, scenario: &Scenario, backend: &mut dyn ShellBackend) -> Result<(), String> {
    let infra = |line: usize, e: anyhow::Error| {
        format!(
            "{}:{line}: infrastructure failure (not an assertion mismatch): {e:#}",
            scenario.path.display()
        )
    };

    let tmp = backend.tmp().to_string();

    // Injected step 0: create the sandbox. On a fresh wasm agent /tmp does not exist;
    // natively the backend pre-created the dir, so this is a no-op there.
    let setup = format!("mkdir -p {tmp}");
    let outcome = backend.eval(&setup).map_err(|e| infra(0, e))?;
    if outcome.exit_code != 0 {
        return Err(format!(
            "{}: sandbox setup `{setup}` failed on {} (exit {})\n--- stdout ---\n{}\n--- stderr ---\n{}",
            scenario.path.display(),
            kind.label(),
            outcome.exit_code,
            outcome.stdout,
            outcome.stderr
        ));
    }

    for (index, step) in scenario.steps.iter().enumerate() {
        let resolved = step.resolved(&tmp);
        let outcome = match &resolved.action {
            Action::Eval(payload) => backend.eval(payload),
            Action::Answer(text) => backend.answer(Some(text)),
            Action::Abort => backend.answer(None),
        }
        .map_err(|e| infra(step.line, e))?;

        let mismatches = matcher::check(&resolved.expect, &outcome);
        if !mismatches.is_empty() {
            return Err(matcher::render_failure(&scenario.path, index, &resolved, &outcome, &mismatches));
        }
    }
    Ok(())
}

/// Fail fast (before any trial) when the golem tier is enabled but no server answers —
/// the alternative is every scenario timing out one by one.
fn preflight() -> Result<(), String> {
    let bin = std::env::var("GOLEM_BIN").unwrap_or_else(|_| "golem".to_string());
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .ok_or("cannot locate repo root")?
        .to_path_buf();

    let mut child = std::process::Command::new(&bin)
        .args(["component", "list", "-q", "--format", "json"])
        .current_dir(&root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run `{bin}`: {e} — is the golem CLI (>=1.5) on PATH?"))?;

    // Poll with a deadline and KILL on timeout — a detached probe would otherwise
    // outlive the harness as an orphan. (`component list` output is far below pipe
    // capacity, so polling before reading cannot deadlock.)
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("`{bin} component list` did not respond within 10s — server hung or unreachable"));
            }
            Err(e) => return Err(format!("waiting on `{bin} component list`: {e}")),
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|e| format!("collecting `{bin} component list` output: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "`{bin} component list` exited with {}\n{}\nStart a server + deploy first: scripts/conformance-golem.sh (or `golem server run` + `golem -Y deploy`).",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}
