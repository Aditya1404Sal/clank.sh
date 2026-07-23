//! The `kill` command: classifier + argument grammar for clank's synthetic kill.
//!
//! On clank there are no OS processes and no signals — `kill` cancels in-shell work (README:
//! "kill terminates or cancels; signal numbers are not mapped"). It is a **Session-layer
//! interception** (like `curl`/`ask`), not a Brush builtin, because the two things it can target
//! are Session state: background jobs (a Brush `Job` whose future gets aborted) and the P-state
//! pending prompt (killing its pid == aborting the prompt). Brush's own `kill` builtin is
//! nix-based and unix-gated; the interception shadows it on native too, so kill semantics are
//! identical on both targets.
//!
//! Grammar: `kill [-s SIG | --signal SIG | -SIG | -N] (<pid> | %<jobspec>)...` — signal
//! specifiers are parsed and **ignored** (every kill is the synthetic terminate). `-l` errors
//! honestly. The actual cancellation lives in `Session::run_kill`.

use brush_parser::{tokenize_str, unquote_str, Token};

/// One kill target.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Target {
    Pid(u32),
    /// A `%`-prefixed jobspec, kept raw for Brush's `resolve_job_spec` (`%1`, `%%`, `%+`, `%-`).
    Job(String),
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct KillArgs {
    pub targets: Vec<Target>,
}

/// Recognize a `kill` line. `None` when the line isn't a kill invocation; `Some(Err)` when it is
/// but the arguments don't parse (usage error).
pub(crate) fn classify(line: &str) -> Option<Result<KillArgs, String>> {
    let tokens = tokenize_str(line).ok()?;
    let mut words = tokens.into_iter().filter_map(|t| match t {
        Token::Word(s, _) => Some(unquote_str(&s)),
        Token::Operator(_, _) => None,
    });
    if words.next()? != "kill" {
        return None;
    }
    Some(parse_args(words))
}

fn parse_args(words: impl Iterator<Item = String>) -> Result<KillArgs, String> {
    let mut args = KillArgs::default();
    let mut words = words.peekable();
    while let Some(word) = words.next() {
        match word.as_str() {
            "-l" | "-L" | "--list" => {
                return Err("signal listing is not supported (signals are not mapped)".into());
            }
            "-s" | "--signal" | "-n" => {
                // Signal by name/number: accepted and ignored (synthetic kill).
                if words.next().is_none() {
                    return Err(format!("option '{word}' requires an argument"));
                }
            }
            w if w.starts_with('%') => args.targets.push(Target::Job(w.to_string())),
            w if w.starts_with('-') && w.len() > 1 => {
                // `-9`, `-KILL`, `-SIGTERM`, `--signal=TERM`: parsed and ignored.
            }
            w => match w.parse::<u32>() {
                Ok(pid) => args.targets.push(Target::Pid(pid)),
                Err(_) => return Err(format!("invalid pid or jobspec '{w}'")),
            },
        }
    }
    if args.targets.is_empty() {
        return Err("usage: kill [-s SIG | -SIG] (<pid> | %<jobspec>)...".into());
    }
    Ok(args)
}

pub(crate) fn manifests() -> Vec<crate::manifest::Manifest> {
    use crate::manifest::{ExecutionScope, Manifest};
    vec![Manifest::builtin("kill", "terminate or cancel in-shell work")
        .with_scope(ExecutionScope::ShellInternal)
        .with_help(
            "kill [-s SIG | -SIG] (<pid> | %<jobspec>)... — cancel a background job (its future \
             is aborted; reported as Killed, exit status 137) or a prompt-paused process (same as \
             aborting the prompt). Signal names/numbers are parsed but NOT mapped — every kill \
             terminates. There are no OS processes in the sandbox.",
        )]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_ignores_non_kill_lines() {
        assert!(classify("echo kill").is_none());
        assert!(classify("killall x").is_none());
        assert!(classify("").is_none());
    }

    #[test]
    fn parses_pids_and_jobspecs() {
        let args = classify("kill 42 %1").unwrap().unwrap();
        assert_eq!(
            args.targets,
            vec![Target::Pid(42), Target::Job("%1".into())]
        );
    }

    #[test]
    fn signal_specifiers_are_accepted_and_ignored() {
        let args = classify("kill -9 42").unwrap().unwrap();
        assert_eq!(args.targets, vec![Target::Pid(42)]);
        let args = classify("kill -s KILL %2").unwrap().unwrap();
        assert_eq!(args.targets, vec![Target::Job("%2".into())]);
        let args = classify("kill -SIGTERM 7").unwrap().unwrap();
        assert_eq!(args.targets, vec![Target::Pid(7)]);
    }

    #[test]
    fn errors_are_honest() {
        assert!(classify("kill").unwrap().unwrap_err().contains("usage"));
        assert!(classify("kill -l").unwrap().unwrap_err().contains("not supported"));
        assert!(classify("kill abc").unwrap().unwrap_err().contains("invalid pid"));
        assert!(classify("kill -s").unwrap().unwrap_err().contains("requires an argument"));
    }
}
