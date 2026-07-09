//! The virtual `/proc` namespace: process files computed on read.
//!
//! `/proc` is **not file-backed** (no bytes on disk) — this module is a pure resolver from a path
//! plus the current [`ProcessTable`] to the file's content, computed fresh on each read. clank's
//! own `cat`/`ls`/`grep` route `/proc` reads through here (delegating real paths to uutils), so the
//! namespace stays virtual while still composing with pipes.
//!
//! Serves:
//! - `/proc/<pid>/cmdline` — the process's argv.
//! - `/proc/<pid>/status`  — a grep-friendly `Key: Value` block.
//! - `/proc/<pid>/environ` — the shell's current environment (see note below).
//! - `/proc/clank/system-prompt` — a placeholder until the `ask` subsystem exists.
//!
//! **`environ` caveat:** processes don't capture their own environment snapshot yet, so
//! `/proc/<pid>/environ` reports the *shell's current* environment (sourced identically to `env`),
//! not a per-process capture. Honest and useful (it's the `GOLEM_*` set on the agent) until
//! per-process env capture lands.
//!
//! **`cmdline` format:** real Linux NUL-separates argv with no trailing newline. clank instead
//! space-joins with a trailing newline — every other clank surface is newline-oriented and
//! LLM-legibility is a first-class constraint. Documented deviation.

use crate::proctable::{ProcRow, ProcessTable};

/// Error resolving a `/proc` path — maps to a `cat`/`grep` "No such file or directory".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcError {
    NotFound(String),
}

/// The virtual root prefix.
const PROC_ROOT: &str = "/proc/";

/// Snapshot the shell's current environment as `(key, value)` pairs — the same source `env`
/// (uu_env) reads, so `/proc/<pid>/environ` and `env` never disagree. Used to populate the
/// `environ` argument to [`resolve`].
pub fn current_environ() -> Vec<(String, String)> {
    std::env::vars().collect()
}

/// Whether `path` is under the virtual `/proc` namespace. (`/proc` itself and `/proc/` count too.)
pub fn is_proc_path(path: &str) -> bool {
    path == "/proc" || path.starts_with(PROC_ROOT)
}

/// Resolve a virtual `/proc` path to its computed content.
///
/// `table` supplies the process rows (including the synthetic root via [`ProcessTable::find`]);
/// `environ` is the shell's current environment as `(key, value)` pairs (rendered sorted).
pub fn resolve(
    path: &str,
    table: &ProcessTable,
    environ: &[(String, String)],
) -> Result<String, ProcError> {
    let not_found = || ProcError::NotFound(path.to_string());

    // Strip the `/proc/` prefix and split into components.
    let rest = path.strip_prefix(PROC_ROOT).ok_or_else(not_found)?;
    let mut parts = rest.split('/').filter(|s| !s.is_empty());
    let first = parts.next().ok_or_else(not_found)?;
    let second = parts.next().ok_or_else(not_found)?;
    // No deeper nesting is served.
    if parts.next().is_some() {
        return Err(not_found());
    }

    // `/proc/clank/<file>` — shell-wide virtual files.
    if first == "clank" {
        return match second {
            "system-prompt" => Ok(system_prompt_stub()),
            _ => Err(not_found()),
        };
    }

    // `/proc/<pid>/<file>` — per-process virtual files.
    let pid: u32 = first.parse().map_err(|_| not_found())?;
    let row = table.find(pid).ok_or_else(not_found)?;
    match second {
        "cmdline" => Ok(cmdline(&row)),
        "status" => Ok(status(&row)),
        "environ" => Ok(environ_block(environ)),
        _ => Err(not_found()),
    }
}

/// `/proc/<pid>/cmdline` — argv space-joined with a trailing newline (LLM-legible; see module docs).
fn cmdline(row: &ProcRow) -> String {
    format!("{}\n", row.command())
}

/// `/proc/<pid>/status` — a grep-friendly `Key: Value` block. `grep State /proc/<pid>/status` works.
fn status(row: &ProcRow) -> String {
    // NOTE: when AgentInvocation rows exist, `/proc/<pid>/status` gains agent-type, agent-params,
    // agent-revision, phantom-uuid, and idempotency-key fields here (README). No such rows yet.
    format!(
        "Pid:\t{}\n\
         PPid:\t{}\n\
         Kind:\t{:?}\n\
         State:\t{} ({})\n\
         Start:\t{}\n\
         Cmd:\t{}\n",
        row.pid,
        row.ppid,
        row.kind,
        row.state.code(),
        row.state.long_name(),
        row.start,
        row.command(),
    )
}

/// `/proc/<pid>/environ` — the shell's current environment as sorted `KEY=VALUE` lines.
fn environ_block(environ: &[(String, String)]) -> String {
    let mut pairs: Vec<&(String, String)> = environ.iter().collect();
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = String::new();
    for (k, v) in pairs {
        out.push_str(&format!("{k}={v}\n"));
    }
    out
}

/// List the fixed child names of a virtual `/proc` directory: `/proc/<pid>` → the per-process files,
/// `/proc/clank` → the shell-wide files. Returns `None` if `dir` isn't a listable `/proc` directory
/// (top-level `/proc` pid enumeration is deferred). Note: this does not validate that `<pid>` exists
/// — `ls /proc/<pid>` lists the file names regardless, matching how `/proc` presents a fixed schema.
pub fn list_children(dir: &str) -> Option<Vec<String>> {
    let rest = dir.strip_prefix(PROC_ROOT)?;
    let trimmed = rest.trim_end_matches('/');
    // Must be a single component (no nested path).
    if trimmed.is_empty() || trimmed.contains('/') {
        return None;
    }
    if trimmed == "clank" {
        return Some(vec!["system-prompt".to_string()]);
    }
    if trimmed.parse::<u32>().is_ok() {
        return Some(vec![
            "cmdline".to_string(),
            "environ".to_string(),
            "status".to_string(),
        ]);
    }
    None
}

/// `/proc/clank/system-prompt` — an honest placeholder until the `ask` subsystem lands. When `ask`
/// arrives, this is the one function body it replaces (per the README, this path is not owned by
/// `ask` specifically — it lives here in the virtual-fs layer).
pub fn system_prompt_stub() -> String {
    "# clank system prompt (placeholder)\n\
     # The system prompt is computed on read from installed tools, skills, and shell\n\
     # configuration. The `ask` subsystem is not yet implemented, so no prompt is\n\
     # generated yet. This path will reflect the real prompt once `ask` lands.\n"
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::ProcessKind;

    fn table_with_one(argv: &str) -> (ProcessTable, u32) {
        let mut t = ProcessTable::new();
        let pid = t.spawn(
            ProcessKind::Builtin,
            argv.split_whitespace().map(String::from).collect(),
        );
        (t, pid)
    }

    fn env() -> Vec<(String, String)> {
        vec![
            ("GOLEM_AGENT_TYPE".into(), "ClankAgent".into()),
            ("HOME".into(), "/home/user".into()),
        ]
    }

    #[test]
    fn is_proc_path_recognizes_proc_only() {
        assert!(is_proc_path("/proc/1/status"));
        assert!(is_proc_path("/proc/clank/system-prompt"));
        assert!(is_proc_path("/proc"));
        assert!(!is_proc_path("/tmp/x"));
        assert!(!is_proc_path("proc/1"));
        assert!(!is_proc_path("/procession"));
    }

    #[test]
    fn cmdline_is_space_joined_argv_with_newline() {
        let (t, pid) = table_with_one("echo hello world");
        let out = resolve(&format!("/proc/{pid}/cmdline"), &t, &env()).unwrap();
        assert_eq!(out, "echo hello world\n");
    }

    #[test]
    fn status_has_grep_friendly_fields() {
        let (t, pid) = table_with_one("ls /tmp");
        let out = resolve(&format!("/proc/{pid}/status"), &t, &env()).unwrap();
        assert!(out.contains("Pid:"));
        assert!(out.contains("PPid:"));
        assert!(out.contains("State:"));
        assert!(out.contains("Cmd:"));
        // Running, since we didn't complete it.
        assert!(out.contains("R (running)"));
        assert!(out.contains("ls /tmp"));
    }

    #[test]
    fn pid_one_resolves_to_synthetic_root() {
        let (t, _pid) = table_with_one("echo x");
        let out = resolve("/proc/1/status", &t, &env()).unwrap();
        assert!(out.contains("Pid:\t1"));
        assert!(out.contains("PPid:\t0"));
        assert!(out.contains("S (sleeping)"));
        assert!(out.contains("clank"));
    }

    #[test]
    fn environ_is_sorted_key_value_lines() {
        let (t, pid) = table_with_one("echo x");
        let out = resolve(&format!("/proc/{pid}/environ"), &t, &env()).unwrap();
        // Sorted: GOLEM_AGENT_TYPE before HOME.
        assert_eq!(out, "GOLEM_AGENT_TYPE=ClankAgent\nHOME=/home/user\n");
    }

    #[test]
    fn system_prompt_returns_placeholder() {
        let (t, _pid) = table_with_one("echo x");
        let out = resolve("/proc/clank/system-prompt", &t, &env()).unwrap();
        assert!(out.contains("system prompt"));
        assert!(out.contains("placeholder"));
    }

    #[test]
    fn unknown_pid_and_paths_are_not_found() {
        let (t, _pid) = table_with_one("echo x");
        assert_eq!(
            resolve("/proc/99999/status", &t, &env()),
            Err(ProcError::NotFound("/proc/99999/status".into()))
        );
        assert!(matches!(
            resolve("/proc/1/bogus", &t, &env()),
            Err(ProcError::NotFound(_))
        ));
        assert!(matches!(
            resolve("/proc/clank/bogus", &t, &env()),
            Err(ProcError::NotFound(_))
        ));
        // Too-deep paths.
        assert!(matches!(
            resolve("/proc/1/status/extra", &t, &env()),
            Err(ProcError::NotFound(_))
        ));
    }

    #[test]
    fn identical_tables_resolve_identically() {
        let (a, pa) = table_with_one("echo a");
        let (b, pb) = table_with_one("echo a");
        assert_eq!(pa, pb);
        assert_eq!(
            resolve(&format!("/proc/{pa}/status"), &a, &env()),
            resolve(&format!("/proc/{pb}/status"), &b, &env())
        );
    }
}
