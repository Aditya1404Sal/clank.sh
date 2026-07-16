//! Expectation matching + failure rendering.
//!
//! `check` returns every mismatch for a step (not just the first), so one failed trial
//! shows the whole picture; `render_failure` formats them with the scenario file:line and
//! the verbatim streams the backend actually produced.

use crate::backend::Outcome;
use crate::scenario::{Action, ExitExpect, Expect, Step, StreamExpect};
use std::fmt::Write as _;

/// Check one stream against its expectation. `label` is `stdout`/`stderr`.
fn check_stream(label: &str, expect: &StreamExpect, got: &str, mismatches: &mut Vec<String>) {
    if expect.any {
        return;
    }
    if let Some(lines) = &expect.exact {
        let want = format!("{}\n", lines.join("\n"));
        if got != want {
            mismatches.push(format!("{label} mismatch\n  expected (exact): {want:?}\n  got:              {got:?}"));
        }
    } else if expect.contains.is_empty() && !got.is_empty() {
        mismatches.push(format!("{label} expected empty\n  got: {got:?}"));
    }
    for needle in &expect.contains {
        if !got.contains(needle) {
            mismatches.push(format!("{label} missing substring {needle:?}\n  got: {got:?}"));
        }
    }
}

/// All mismatches between a step's expectation and the backend's outcome; empty = pass.
pub fn check(expect: &Expect, outcome: &Outcome) -> Vec<String> {
    let mut mismatches = Vec::new();

    check_stream("stdout", &expect.stdout, &outcome.stdout, &mut mismatches);
    check_stream("stderr", &expect.stderr, &outcome.stderr, &mut mismatches);

    if let ExitExpect::Code(want) = expect.exit {
        if outcome.exit_code != want {
            mismatches.push(format!("exit code: expected {want}, got {}", outcome.exit_code));
        }
    }

    match (&expect.prompt, &outcome.pending) {
        (None, Some(p)) if expect.choices.is_none() => {
            mismatches.push(format!(
                "unexpected pending prompt: {:?} (steps assert NO pending unless `prompt~` is stated)",
                p.question
            ));
        }
        (Some(want), None) => {
            mismatches.push(format!("expected a pending prompt containing {want:?}, got none"));
        }
        (Some(want), Some(p)) if !p.question.contains(want.as_str()) => {
            mismatches.push(format!(
                "pending prompt question mismatch\n  expected to contain: {want:?}\n  got: {:?}",
                p.question
            ));
        }
        _ => {}
    }

    if let Some(want) = &expect.choices {
        match &outcome.pending {
            None => mismatches.push(format!("expected pending prompt choices {want:?}, but no prompt is pending")),
            Some(p) if p.choices.as_ref() != Some(want) => {
                mismatches.push(format!("pending prompt choices: expected {want:?}, got {:?}", p.choices));
            }
            _ => {}
        }
    }

    mismatches
}

/// Render a failed step as the trial's failure message.
pub fn render_failure(scenario_path: &std::path::Path, step_index: usize, step: &Step, outcome: &Outcome, mismatches: &[String]) -> String {
    let action = match &step.action {
        Action::Eval(p) => format!("run {p}"),
        Action::Answer(t) => format!("answer {t}"),
        Action::Abort => "abort".to_string(),
    };
    let mut out = String::new();
    let _ = writeln!(out, "{}:{}", scenario_path.display(), step.line);
    let _ = writeln!(out, "step {}: {}", step_index + 1, action.replace('\n', "\n        + "));
    for m in mismatches {
        let _ = writeln!(out, "{m}");
    }
    let _ = writeln!(out, "--- got stdout ---\n{}", outcome.stdout);
    let _ = writeln!(out, "--- got stderr ---\n{}", outcome.stderr);
    let _ = write!(
        out,
        "exit: {}   pending: {}",
        outcome.exit_code,
        match &outcome.pending {
            Some(p) => format!("{:?} choices {:?}", p.question, p.choices),
            None => "none".to_string(),
        }
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::PendingView;
    use crate::scenario::parse;
    use std::path::Path;

    fn outcome(stdout: &str, stderr: &str, exit_code: u8, pending: Option<PendingView>) -> Outcome {
        Outcome { stdout: stdout.into(), stderr: stderr.into(), exit_code, pending }
    }

    fn expect_of(text: &str) -> Expect {
        parse("t", Path::new("t.clank"), text).unwrap().steps[0].expect.clone()
    }

    #[test]
    fn exact_match_passes_and_fails() {
        let e = expect_of("run echo hi\nout hi\n");
        assert!(check(&e, &outcome("hi\n", "", 0, None)).is_empty());
        let m = check(&e, &outcome("hi \n", "", 0, None));
        assert_eq!(m.len(), 1);
        assert!(m[0].contains("stdout mismatch"), "{m:?}");
    }

    #[test]
    fn default_expects_empty_streams_exit_zero_no_prompt() {
        let e = expect_of("run true\n");
        assert!(check(&e, &outcome("", "", 0, None)).is_empty());
        let m = check(
            &e,
            &outcome("x\n", "boom\n", 3, Some(PendingView { question: "Q?".into(), choices: None })),
        );
        // stdout non-empty, stderr non-empty, exit != 0, unexpected prompt: all four reported.
        assert_eq!(m.len(), 4, "{m:?}");
    }

    #[test]
    fn contains_and_any_relax_the_stream_check() {
        let e = expect_of("run wc\nout~ 3\nerr* \nexit any\n");
        assert!(check(&e, &outcome("      3 f\n", "noise", 2, None)).is_empty());
        let m = check(&e, &outcome("nope\n", "", 0, None));
        assert_eq!(m.len(), 1, "{m:?}");
        assert!(m[0].contains("missing substring"), "{m:?}");
    }

    #[test]
    fn prompt_and_choices_assertions() {
        let e = expect_of("run p\nout* \nprompt~ Deploy\nchoices staging,production\n");
        let good = PendingView {
            question: "Deploy?".into(),
            choices: Some(vec!["staging".into(), "production".into()]),
        };
        assert!(check(&e, &outcome("", "", 0, Some(good))).is_empty());
        let m = check(&e, &outcome("", "", 0, None));
        assert_eq!(m.len(), 2, "{m:?}"); // missing prompt + missing choices
    }

    #[test]
    fn multiline_exact_block() {
        let e = expect_of("run x\nout a\nout b\n");
        assert!(check(&e, &outcome("a\nb\n", "", 0, None)).is_empty());
        assert!(!check(&e, &outcome("a\nb", "", 0, None)).is_empty()); // trailing \n required
    }
}
