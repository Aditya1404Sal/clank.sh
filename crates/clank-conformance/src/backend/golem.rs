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
//! - [`decode_invoke`] — `golem agent invoke -q --format json` output on golem 1.5.x:
//!   the LAST line carrying a `result_json` document, whose `.result_json.value` is the
//!   agent's eval-result record with NAMED fields.

use super::{Outcome, PendingView, ShellBackend};
use anyhow::{bail, Context};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

pub struct GolemBackend {
    rt: tokio::runtime::Runtime,
    agent_id: String,
    repo_root: PathBuf,
    golem_bin: String,
    timeout: Duration,
}

impl GolemBackend {
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
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(60));

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
            .map(|s| s.to_string())
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

    fn tmp(&self) -> &str {
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
                out.push_str(&format!("\\u{{{:x}}}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Decode `golem agent invoke -q --format json` output into an [`Outcome`].
///
/// The golem 1.5.x CLI prints invocation markers and stream lines first, then the result
/// document; the e2e's proven recipe is "last line containing `result_json`". A whole-
/// output parse is the fallback for a pretty-printed document.
pub fn decode_invoke(cli_stdout: &str) -> anyhow::Result<Outcome> {
    let mut doc: Option<serde_json::Value> = None;
    for line in cli_stdout.lines() {
        if !line.contains("\"result_json\"") {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            if v.get("result_json").is_some() {
                doc = Some(v);
            }
        }
    }
    let doc = match doc {
        Some(d) => d,
        None => serde_json::from_str::<serde_json::Value>(cli_stdout)
            .ok()
            .filter(|v| v.get("result_json").is_some())
            .context("no `result_json` document in golem CLI output — is clank deployed on this server?")?,
    };

    let value = &doc["result_json"]["value"];
    if value.is_null() {
        bail!("`result_json` document carries no value: {doc}");
    }

    let exit_code = value["exit_code"]
        .as_u64()
        .context("eval-result has no numeric `exit_code` — named-field decode failed (dev-CLI positional output?)")?;

    let pending = match &value["pending_prompt"] {
        serde_json::Value::Null => None,
        p => Some(PendingView {
            question: p["question"].as_str().unwrap_or_default().to_string(),
            choices: p["choices"].as_array().map(|a| {
                a.iter()
                    .filter_map(|c| c.as_str().map(str::to_string))
                    .collect()
            }),
        }),
    };

    Ok(Outcome {
        stdout: value["stdout"].as_str().unwrap_or_default().to_string(),
        stderr: value["stderr"].as_str().unwrap_or_default().to_string(),
        exit_code: u8::try_from(exit_code).with_context(|| format!("exit_code {exit_code} out of u8 range"))?,
        pending,
    })
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
    fn missing_result_json_is_an_error_not_empty() {
        // The dev-CLI rename (`resultJson`) silently emptied every e2e reader; here it
        // must be a loud infrastructure error instead.
        let err = decode_invoke(r#"{"resultJson":{"value":[]}}"#).unwrap_err();
        assert!(err.to_string().contains("no `result_json`"), "{err}");
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
