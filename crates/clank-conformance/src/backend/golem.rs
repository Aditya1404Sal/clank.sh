//! The wasm target: a deployed clank agent driven through the `golem` CLI.
//!
//! Each scenario gets a FRESH agent instance (`ClankAgent("conf-<stem>-<pid>")`), which on
//! Golem means a fresh isolated VFS — scenarios can never couple through shared state the
//! way the monolithic e2e's single instance does.
//!
//! Two contracts live here as free, unit-tested functions:
//! - [`wit_string`] — the payload is a WAVE/WIT string literal. Valid escapes are ONLY
//!   `\\ \" \n \r \t \u{…}`; a raw `\` degrades the whole literal and `\$` is invalid
//!   (the e2e's documented `$`-trap), so escaping happens totally, here, in one place.
//! - [`decode_invoke`] — `golem agent invoke -q --format json` output. Accepts BOTH the
//!   released CLI's `result_json` document (named-field record) and the dev SDK CLI's
//!   `resultJson` document (positional schema-value-tree), taking the LAST result line.

use super::{Outcome, PendingView, ShellBackend};
use anyhow::{bail, Context};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

/// A [`ShellBackend`] that drives a deployed clank agent through the `golem` CLI.
pub struct GolemBackend {
    rt: tokio::runtime::Runtime,
    agent_id: String,
    repo_root: PathBuf,
    golem_bin: String,
    timeout: Duration,
}

impl GolemBackend {
    /// Construct a backend that drives a fresh clank agent per scenario via the `golem` CLI.
    ///
    /// # Errors
    /// Returns `Err` if `scenario` is not a non-empty `[a-z0-9-]` string, if the repo root
    /// cannot be located, or if the tokio runtime fails to build.
    pub fn new(scenario: &str) -> anyhow::Result<Self> {
        // The stem goes verbatim into the agent id — lossy sanitization could collapse
        // two distinct scenarios onto ONE durable agent (silent state sharing), so
        // reject instead of mangling. The harness enforces this at discovery too.
        if scenario.is_empty()
            || !scenario.chars().all(|c| matches!(c, 'a'..='z' | '0'..='9' | '-'))
        {
            bail!("scenario name `{scenario}` must be non-empty [a-z0-9-] (it becomes the agent id)");
        }
        let agent_id = format!("ClankAgent(\"conf-{scenario}-{}\")", std::process::id());

        // crates/clank-conformance -> crates -> repo root; the CLI resolves the app
        // context (golem.yaml) from the cwd like the e2e does.
        let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .context("locating repo root")?
            .to_path_buf();

        let timeout = std::env::var("CLANK_CONFORMANCE_STEP_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .map_or(Duration::from_mins(1), Duration::from_secs);

        Ok(Self {
            rt: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("building tokio runtime")?,
            agent_id,
            repo_root,
            golem_bin: std::env::var("GOLEM_BIN").unwrap_or_else(|_| "golem".to_string()),
            timeout,
        })
    }

    fn invoke(&self, method: &str, payload: Option<&str>) -> anyhow::Result<Outcome> {
        let mut argv: Vec<String> = ["agent", "invoke", "-q", "--format", "json"]
            .iter()
            .map(std::string::ToString::to_string)
            .collect();
        argv.push(self.agent_id.clone());
        argv.push(method.to_string());
        if let Some(p) = payload {
            argv.push(wit_string(p));
        }

        let output = self.rt.block_on(async {
            let child = tokio::process::Command::new(&self.golem_bin)
                .args(&argv)
                .current_dir(&self.repo_root)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .with_context(|| format!("spawning `{}` — is the golem CLI on PATH?", self.golem_bin))?;
            match tokio::time::timeout(self.timeout, child.wait_with_output()).await {
                Ok(result) => result.context("collecting golem CLI output"),
                Err(_) => bail!(
                    "`{} {}` timed out after {:?} (agent {}) — the invocation may still be \
                     running server-side",
                    self.golem_bin,
                    method,
                    self.timeout,
                    self.agent_id
                ),
            }
        })?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !output.status.success() {
            bail!(
                "`{} agent invoke … {method}` exited with {}\n--- CLI stderr ---\n{}\n--- CLI stdout ---\n{}",
                self.golem_bin,
                output.status,
                stderr,
                stdout
            );
        }
        decode_invoke(&stdout)
            .with_context(|| format!("decoding `{method}` result for {}\n--- CLI stderr ---\n{stderr}", self.agent_id))
    }
}

impl ShellBackend for GolemBackend {
    fn eval(&mut self, line: &str) -> anyhow::Result<Outcome> {
        self.invoke("eval", Some(line))
    }

    fn answer(&mut self, response: Option<&str>) -> anyhow::Result<Outcome> {
        match response {
            Some(text) => self.invoke("answer_prompt", Some(text)),
            None => self.invoke("abort_prompt", None),
        }
    }

    fn tmp(&self) -> &'static str {
        // A fresh agent per scenario means a fresh VFS — a fixed path is collision-free.
        // (A fresh agent has NO /tmp; the harness's injected `mkdir -p` step creates it.)
        "/tmp/w"
    }

    fn finish(self: Box<Self>) -> anyhow::Result<()> {
        // The instance is left in place deliberately: with --keep on the wrapper script,
        // `golem agent list` + per-scenario names make post-mortems cheap.
        Ok(())
    }
}

/// Render `s` as a WAVE/WIT string literal (quotes included).
///
/// `$` is NOT escaped — `\$` is an invalid WAVE escape that silently degrades the whole
/// argument to a raw string (the e2e's documented trap). Control characters use `\u{…}`.
#[must_use]
pub fn wit_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{{{:x}}}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Decode `golem agent invoke -q --format json` output into an [`Outcome`].
///
/// The CLI prints invocation markers and stream lines first, then the result document; the
/// proven recipe is "last line carrying a result document". A whole-output parse is the
/// fallback for a pretty-printed document. Both wire shapes are accepted:
/// - **released** golem 1.5.x — a `result_json` (`snake_case`) document whose value is the
///   eval-result record with NAMED fields (`.stdout`/`.exit_code`/…); and
/// - **dev SDK** — a `resultJson` (`camelCase`) document whose value is a POSITIONAL
///   schema-value-tree (`.value.value.fields[0..3]`, each a `{ "value": … }` node in
///   declaration order). This mirrors `scripts/golem-e2e.sh`'s `EVAL_REMAP`.
///
/// # Errors
/// Returns `Err` if no result document is present, if it carries no value, if the value
/// matches neither shape, if the exit code isn't numeric, or if it's outside `u8` range.
pub fn decode_invoke(cli_stdout: &str) -> anyhow::Result<Outcome> {
    let mut doc: Option<serde_json::Value> = None;
    for line in cli_stdout.lines() {
        if !line.contains("\"result_json\"") && !line.contains("\"resultJson\"") {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            if result_value(&v).is_some() {
                doc = Some(v);
            }
        }
    }
    let doc = match doc {
        Some(d) => d,
        None => serde_json::from_str::<serde_json::Value>(cli_stdout)
            .ok()
            .filter(|v| result_value(v).is_some())
            .context(
                "no `result_json`/`resultJson` document in golem CLI output — \
                 is clank deployed on this server?",
            )?,
    };

    let value = result_value(&doc)
        .filter(|v| !v.is_null())
        .with_context(|| format!("result document carries no value: {doc}"))?;
    decode_eval_value(value).with_context(|| format!("decoding eval-result from {doc}"))
}

/// The eval-result `value` node, under whichever key the active golem CLI uses:
/// `result_json` (released 1.5.x) or `resultJson` (dev SDK).
fn result_value(doc: &serde_json::Value) -> Option<&serde_json::Value> {
    doc.get("result_json")
        .or_else(|| doc.get("resultJson"))
        .map(|outer| &outer["value"])
}

/// Decode an eval-result `value` node into an [`Outcome`], accepting the released CLI's
/// NAMED-field record and the dev SDK's POSITIONAL schema-value-tree.
fn decode_eval_value(value: &serde_json::Value) -> anyhow::Result<Outcome> {
    // Released shape: `exit_code`/`stdout`/… are named fields directly on `value`.
    if let Some(exit_code) = value["exit_code"].as_u64() {
        return Ok(Outcome {
            stdout: value["stdout"].as_str().unwrap_or_default().to_string(),
            stderr: value["stderr"].as_str().unwrap_or_default().to_string(),
            exit_code: u8::try_from(exit_code)
                .with_context(|| format!("exit_code {exit_code} out of u8 range"))?,
            pending: decode_pending_named(&value["pending_prompt"]),
        });
    }

    // Dev SDK shape: `value.value.fields` is a positional array of `{ "value": … }` nodes,
    // in declaration order — stdout, stderr, exit_code, pending_prompt (a tagged option).
    let fields = value["value"]["fields"].as_array().context(
        "eval-result has neither a named `exit_code` nor a positional `value.fields` array \
         — unrecognized golem CLI output shape",
    )?;
    let str_field = |i: usize| {
        fields
            .get(i)
            .and_then(|f| f["value"].as_str())
            .unwrap_or_default()
            .to_string()
    };
    let exit_code = fields
        .get(2)
        .and_then(|f| f["value"].as_u64())
        .context("positional eval-result field 2 (exit_code) is not a number")?;
    Ok(Outcome {
        stdout: str_field(0),
        stderr: str_field(1),
        exit_code: u8::try_from(exit_code)
            .with_context(|| format!("exit_code {exit_code} out of u8 range"))?,
        pending: fields.get(3).and_then(|f| decode_pending_positional(&f["value"])),
    })
}

/// Released shape: `pending_prompt` is null-or-record with named `question`/`choices`.
fn decode_pending_named(p: &serde_json::Value) -> Option<PendingView> {
    if p.is_null() {
        return None;
    }
    Some(PendingView {
        question: p["question"].as_str().unwrap_or_default().to_string(),
        choices: p["choices"]
            .as_array()
            .map(|a| a.iter().filter_map(|c| c.as_str().map(str::to_string)).collect()),
    })
}

/// Dev SDK shape: the pending-prompt option node — `.inner` is null when absent, else a
/// positional record `[question, choices]`, `choices` itself a tagged option of a list.
fn decode_pending_positional(p: &serde_json::Value) -> Option<PendingView> {
    let inner = p.get("inner")?;
    if inner.is_null() {
        return None;
    }
    let fields = inner["value"]["fields"].as_array()?;
    let question = fields
        .first()
        .and_then(|f| f["value"].as_str())
        .unwrap_or_default()
        .to_string();
    let choices = fields.get(1).and_then(|f| {
        let ci = f["value"].get("inner")?;
        if ci.is_null() {
            return None;
        }
        Some(
            ci["value"]["elements"]
                .as_array()
                .map_or_else(Vec::new, |els| {
                    els.iter()
                        .filter_map(|e| e["value"].as_str().map(str::to_string))
                        .collect()
                }),
        )
    });
    Some(PendingView { question, choices })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wit_string_escapes_totally() {
        assert_eq!(wit_string("echo hi"), r#""echo hi""#);
        assert_eq!(wit_string(r#"echo "q""#), r#""echo \"q\"""#);
        // Backslash first — the e2e's single-`"`-escape was insufficient here.
        assert_eq!(wit_string(r"grep 'a\.b' f"), r#""grep 'a\\.b' f""#);
        // `$` passes through raw: `\$` is an invalid WAVE escape (the documented trap).
        assert_eq!(wit_string("echo $HOME ${x}"), r#""echo $HOME ${x}""#);
        assert_eq!(wit_string("a\nb\tc"), r#""a\nb\tc""#);
        assert_eq!(wit_string("bell\u{7}"), r#""bell\u{7}""#);
    }

    #[test]
    fn decodes_a_result_document() {
        // Shaped like golem 1.5.x `agent invoke -q --format json` output: marker lines,
        // then the result document on one line (the e2e's `grep result_json | tail -1`).
        let cli = concat!(
            "some invocation marker\n",
            r#"{"result_json":{"typ":{"items":[]},"value":{"stdout":"hi\n","stderr":"","exit_code":0,"pending_prompt":null}}}"#,
            "\n"
        );
        let o = decode_invoke(cli).unwrap();
        assert_eq!(o.stdout, "hi\n");
        assert_eq!(o.exit_code, 0);
        assert!(o.pending.is_none());
    }

    #[test]
    fn decodes_a_pending_prompt_with_choices() {
        let cli = r#"{"result_json":{"value":{"stdout":"Deploy?\n","stderr":"","exit_code":0,"pending_prompt":{"question":"Deploy?","choices":["staging","production"],"secret":false}}}}"#;
        let o = decode_invoke(cli).unwrap();
        let p = o.pending.expect("pending");
        assert_eq!(p.question, "Deploy?");
        assert_eq!(p.choices.as_deref(), Some(&["staging".to_string(), "production".to_string()][..]));
    }

    #[test]
    fn missing_result_document_is_a_loud_error() {
        // No result key at all → a loud infrastructure error, never a silently-empty Outcome.
        let err = decode_invoke(r#"{"nope":123}"#).unwrap_err();
        assert!(err.to_string().contains("no `result_json`/`resultJson`"), "{err}");
    }

    #[test]
    fn malformed_dev_document_errors_not_empty() {
        // A `resultJson` doc whose value matches NEITHER shape must still error loudly — the
        // dev-CLI rename once silently emptied every reader, and that must never recur.
        let err = decode_invoke(r#"{"resultJson":{"value":[]}}"#).unwrap_err();
        // The specific cause is a `with_context` layer, so check the full chain (`{:#}`).
        assert!(format!("{err:#}").contains("unrecognized golem CLI output shape"), "{err:#}");
    }

    #[test]
    fn decodes_the_dev_sdk_positional_shape() {
        // Dev SDK: camelCase `resultJson`, value is a positional schema-value-tree
        // (stdout, stderr, exit_code, pending_prompt) — mirrors golem-e2e.sh's EVAL_REMAP.
        let cli = concat!(
            "some invocation marker\n",
            r#"{"resultJson":{"value":{"value":{"fields":[{"value":"hi\n"},{"value":""},{"value":0},{"value":{"inner":null}}]}}}}"#,
            "\n"
        );
        let o = decode_invoke(cli).unwrap();
        assert_eq!(o.stdout, "hi\n");
        assert_eq!(o.exit_code, 0);
        assert!(o.pending.is_none());
    }

    #[test]
    fn decodes_a_dev_sdk_pending_prompt_with_choices() {
        // The pending_prompt option (field 3) present, with a choices list.
        let cli = concat!(
            r#"{"resultJson":{"value":{"value":{"fields":["#,
            r#"{"value":"Deploy?\n"},{"value":""},{"value":0},"#,
            r#"{"value":{"inner":{"value":{"fields":["#,
            r#"{"value":"Deploy?"},"#,
            r#"{"value":{"inner":{"value":{"elements":[{"value":"staging"},{"value":"production"}]}}}}"#,
            r#"]}}}}]}}}}"#,
            "\n"
        );
        let o = decode_invoke(cli).unwrap();
        let p = o.pending.expect("pending");
        assert_eq!(p.question, "Deploy?");
        assert_eq!(p.choices.as_deref(), Some(&["staging".to_string(), "production".to_string()][..]));
    }

    #[test]
    fn takes_the_last_result_json_line() {
        let cli = concat!(
            r#"{"result_json":{"value":{"stdout":"old\n","stderr":"","exit_code":1,"pending_prompt":null}}}"#,
            "\n",
            r#"{"result_json":{"value":{"stdout":"new\n","stderr":"","exit_code":0,"pending_prompt":null}}}"#,
            "\n"
        );
        assert_eq!(decode_invoke(cli).unwrap().stdout, "new\n");
    }
}
