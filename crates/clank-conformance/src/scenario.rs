//! Parser for `.clank` scenario files.
//!
//! The grammar is line-oriented and deliberately tiny (spec: `scenarios/README.md`).
//! Keyword, then a SINGLE space, then a verbatim payload — everything after the first
//! space is content, so expected output may carry significant leading whitespace
//! (`out       3 ${TMP}/f` asserts uu `wc`'s padding exactly).

use crate::backend::BackendKind;
use std::fmt;
use std::path::{Path, PathBuf};

/// One parsed scenario file: an ordered list of steps sharing a single shell session.
#[derive(Debug, Clone, PartialEq)]
pub struct Scenario {
    pub name: String,
    pub path: PathBuf,
    /// `@only native` / `@only golem` — restrict the scenario to one backend.
    pub only: Option<BackendKind>,
    /// `@requires <tier>` tags (network/llm/grease/mcp). Parsed now; scenarios carrying
    /// any tag are reported `ignored` until the gated tiers are wired up.
    pub requires: Vec<String>,
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    /// 1-based line number of the step's action line, for failure reports.
    pub line: usize,
    pub action: Action,
    pub expect: Expect,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// `run <line>` (+ `+ <line>` continuations) → one `eval` invocation.
    Eval(String),
    /// `answer <text>` → `answer_prompt`.
    Answer(String),
    /// `abort` → `abort_prompt`.
    Abort,
}

/// What a step asserts about its outcome. Defaults are strict: empty stdout, empty stderr,
/// exit 0, no pending prompt — every deviation must be stated in the file.
#[derive(Debug, Clone, PartialEq)]
pub struct Expect {
    pub stdout: StreamExpect,
    pub stderr: StreamExpect,
    pub exit: ExitExpect,
    /// `prompt~ <substr>` — the outcome must carry a pending prompt whose question
    /// contains the substring. `None` asserts NO pending prompt (the default), which is
    /// what catches accidental pauses.
    pub prompt: Option<String>,
    /// `choices <a,b,c>` — the pending prompt's choice list, exactly.
    pub choices: Option<Vec<String>>,
}

impl Default for Expect {
    fn default() -> Self {
        Self {
            stdout: StreamExpect::default(),
            stderr: StreamExpect::default(),
            exit: ExitExpect::Code(0),
            prompt: None,
            choices: None,
        }
    }
}

/// Expectation over one output stream.
///
/// - `any` (`out*`) — stream unasserted; may not be combined with the other two.
/// - `exact` (`out <line>`…) — lines accumulate; the stream must equal them joined with
///   `\n` plus a trailing `\n`.
/// - `contains` (`out~ <substr>`…) — each substring must appear somewhere in the stream.
/// - none of the above — the stream must be exactly empty.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct StreamExpect {
    pub exact: Option<Vec<String>>,
    pub contains: Vec<String>,
    pub any: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitExpect {
    Code(u8),
    Any,
}

/// A parse failure, pointing at the offending file line. Parse errors surface as failing
/// trials (never silent skips), so the message must stand alone.
#[derive(Debug)]
pub struct ParseError {
    pub path: PathBuf,
    pub line: usize,
    pub msg: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}: {}", self.path.display(), self.line, self.msg)
    }
}

impl std::error::Error for ParseError {}

/// Substitute the runtime variables into scenario text. `${TMP}` is the per-scenario
/// sandbox dir — substituted in payloads AND expectation text so paths echo back verbatim.
pub fn substitute(text: &str, tmp: &str) -> String {
    text.replace("${TMP}", tmp)
}

impl Step {
    /// A copy of this step with runtime variables resolved.
    pub fn resolved(&self, tmp: &str) -> Step {
        let sub = |s: &str| substitute(s, tmp);
        Step {
            line: self.line,
            action: match &self.action {
                Action::Eval(p) => Action::Eval(sub(p)),
                Action::Answer(t) => Action::Answer(sub(t)),
                Action::Abort => Action::Abort,
            },
            expect: Expect {
                stdout: self.expect.stdout.resolved(tmp),
                stderr: self.expect.stderr.resolved(tmp),
                exit: self.expect.exit,
                prompt: self.expect.prompt.as_deref().map(sub),
                choices: self
                    .expect
                    .choices
                    .as_ref()
                    .map(|v| v.iter().map(|c| sub(c)).collect()),
            },
        }
    }
}

impl StreamExpect {
    fn resolved(&self, tmp: &str) -> StreamExpect {
        StreamExpect {
            exact: self
                .exact
                .as_ref()
                .map(|v| v.iter().map(|l| substitute(l, tmp)).collect()),
            contains: self.contains.iter().map(|s| substitute(s, tmp)).collect(),
            any: self.any,
        }
    }
}

/// Split a scenario line into `(keyword, payload)`. The separator is a single ASCII
/// space; everything after it is verbatim payload (leading whitespace significant).
/// `None` means the separator itself is absent — distinct from an empty payload
/// (`out ` with a trailing space asserts one empty line; bare `out` is a grammar error).
fn split_keyword(line: &str) -> (&str, Option<&str>) {
    match line.find(' ') {
        Some(i) => (&line[..i], Some(&line[i + 1..])),
        None => (line, None),
    }
}

pub fn parse(name: &str, path: &Path, text: &str) -> Result<Scenario, ParseError> {
    let err = |line: usize, msg: String| ParseError {
        path: path.to_path_buf(),
        line,
        msg,
    };

    let mut scenario = Scenario {
        name: name.to_string(),
        path: path.to_path_buf(),
        only: None,
        requires: Vec::new(),
        steps: Vec::new(),
    };
    // True while the last step is an Eval still accepting `+` continuations (i.e. no
    // expectation directive, blank line, or comment has followed it yet) — keeps payload
    // and expectations from interleaving, and makes a blank line inside an intended
    // heredoc body a LOUD error on the next `+` instead of silently vanishing from the
    // command under test (a blank content line is spelled as a lone `+`).
    let mut payload_open = false;
    // Whether the current step has already stated `exit` — `ExitExpect::Code(0)` is also
    // the default, so the value alone can't detect a duplicate.
    let mut exit_seen = false;

    for (idx, raw) in text.lines().enumerate() {
        let n = idx + 1;
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            payload_open = false;
            continue;
        }

        let (keyword, payload) = split_keyword(line);

        // Header directives — only before the first step.
        if keyword.starts_with('@') {
            if !scenario.steps.is_empty() {
                return Err(err(n, format!("`{keyword}` must appear before the first step")));
            }
            match keyword {
                "@only" => {
                    if scenario.only.is_some() {
                        return Err(err(n, "`@only` stated twice".into()));
                    }
                    scenario.only = Some(match payload.unwrap_or_default().trim() {
                        "native" => BackendKind::Native,
                        "golem" => BackendKind::Golem,
                        other => {
                            return Err(err(n, format!("`@only` takes `native` or `golem`, got `{other}`")))
                        }
                    });
                }
                "@requires" => {
                    let tag = payload.unwrap_or_default().trim();
                    if tag.is_empty() {
                        return Err(err(n, "`@requires` needs a tier tag".into()));
                    }
                    scenario.requires.push(tag.to_string());
                }
                other => return Err(err(n, format!("unknown header directive `{other}`"))),
            }
            continue;
        }

        // Action directives start a new step.
        match keyword {
            "run" => {
                let cmd = payload.unwrap_or_default();
                if cmd.trim().is_empty() {
                    return Err(err(n, "`run` needs a command line".into()));
                }
                scenario.steps.push(Step {
                    line: n,
                    action: Action::Eval(cmd.to_string()),
                    expect: Expect::default(),
                });
                payload_open = true;
                exit_seen = false;
                continue;
            }
            "+" => {
                if !payload_open {
                    return Err(err(
                        n,
                        "`+` continuation must directly follow `run` (no blank/comment lines or expectations between; a blank content line is a lone `+`)".into(),
                    ));
                }
                if let Some(Step { action: Action::Eval(p), .. }) = scenario.steps.last_mut() {
                    p.push('\n');
                    // Bare `+` = a blank line of payload (e.g. an empty line in a heredoc body).
                    p.push_str(payload.unwrap_or_default());
                } else {
                    unreachable!("payload_open implies a trailing Eval step");
                }
                continue;
            }
            "answer" => {
                let Some(text) = payload else {
                    return Err(err(n, "`answer` needs a payload (use `answer ` with a trailing space for an empty answer)".into()));
                };
                scenario.steps.push(Step {
                    line: n,
                    action: Action::Answer(text.to_string()),
                    expect: Expect::default(),
                });
                payload_open = false;
                exit_seen = false;
                continue;
            }
            "abort" => {
                if !payload.unwrap_or_default().trim().is_empty() {
                    return Err(err(n, "`abort` takes no payload".into()));
                }
                scenario.steps.push(Step {
                    line: n,
                    action: Action::Abort,
                    expect: Expect::default(),
                });
                payload_open = false;
                exit_seen = false;
                continue;
            }
            _ => {}
        }

        // Everything else is an expectation directive on the current step.
        let step = scenario.steps.last_mut().ok_or_else(|| {
            err(n, format!("`{keyword}` before any `run`/`answer`/`abort` step"))
        })?;
        payload_open = false;

        match keyword {
            "out" | "err" => {
                // The separator must be present: `out ` (trailing space) asserts one empty
                // line; a bare `out` is a forgotten payload, not an assertion.
                let Some(text) = payload else {
                    return Err(err(n, format!("`{keyword}` needs a payload (use `{keyword} ` with a trailing space for an empty line)")));
                };
                let stream = if keyword == "out" { &mut step.expect.stdout } else { &mut step.expect.stderr };
                stream.exact.get_or_insert_with(Vec::new).push(text.to_string());
            }
            "out~" | "err~" => {
                let text = payload.unwrap_or_default();
                if text.is_empty() {
                    return Err(err(n, format!("`{keyword}` needs a substring")));
                }
                let stream = if keyword == "out~" { &mut step.expect.stdout } else { &mut step.expect.stderr };
                stream.contains.push(text.to_string());
            }
            "out*" | "err*" => {
                if !payload.unwrap_or_default().trim().is_empty() {
                    return Err(err(n, format!("`{keyword}` takes no payload")));
                }
                let stream = if keyword == "out*" { &mut step.expect.stdout } else { &mut step.expect.stderr };
                stream.any = true;
            }
            "exit" => {
                if exit_seen {
                    return Err(err(n, "`exit` stated twice for one step".into()));
                }
                exit_seen = true;
                step.expect.exit = match payload.unwrap_or_default().trim() {
                    "any" => ExitExpect::Any,
                    num => ExitExpect::Code(num.parse().map_err(|_| {
                        err(n, format!("`exit` takes a number 0-255 or `any`, got `{num}`"))
                    })?),
                };
            }
            "prompt~" => {
                let text = payload.unwrap_or_default();
                if text.is_empty() {
                    return Err(err(n, "`prompt~` needs a substring".into()));
                }
                if step.expect.prompt.is_some() {
                    return Err(err(n, "`prompt~` stated twice for one step".into()));
                }
                step.expect.prompt = Some(text.to_string());
            }
            "choices" => {
                if step.expect.choices.is_some() {
                    return Err(err(n, "`choices` stated twice for one step".into()));
                }
                // Items are VERBATIM between commas (no trimming) per the payload rule;
                // a choice containing a comma is inexpressible — documented in README.
                let list: Vec<String> = payload.unwrap_or_default().split(',').map(str::to_string).collect();
                if list.iter().any(|c| c.is_empty()) {
                    return Err(err(n, "`choices` takes a non-empty comma-separated list".into()));
                }
                step.expect.choices = Some(list);
            }
            other => return Err(err(n, format!("unknown directive `{other}`"))),
        }
    }

    if scenario.steps.is_empty() {
        return Err(err(1, "scenario has no steps".into()));
    }
    // `out*` contradicts `out`/`out~` (and same for err) — reject the combination.
    for step in &scenario.steps {
        for (label, stream) in [("out", &step.expect.stdout), ("err", &step.expect.stderr)] {
            if stream.any && (stream.exact.is_some() || !stream.contains.is_empty()) {
                return Err(err(
                    step.line,
                    format!("`{label}*` cannot be combined with `{label}`/`{label}~` on one step"),
                ));
            }
        }
    }

    Ok(scenario)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(text: &str) -> Scenario {
        parse("t", Path::new("t.clank"), text).expect("parse")
    }

    fn parse_err(text: &str) -> ParseError {
        parse("t", Path::new("t.clank"), text).expect_err("expected parse error")
    }

    #[test]
    fn parses_a_full_scenario() {
        let s = parse_ok(
            "# comment\n\
             @only golem\n\
             @requires network\n\
             \n\
             run echo hi\n\
             out hi\n\
             \n\
             run prompt-user \"Q?\" --choices a,b\n\
             out~ Q?\n\
             prompt~ Q?\n\
             choices a,b\n\
             \n\
             answer a\n\
             out a\n\
             \n\
             abort\n\
             exit 130\n",
        );
        assert_eq!(s.only, Some(BackendKind::Golem));
        assert_eq!(s.requires, vec!["network"]);
        assert_eq!(s.steps.len(), 4);
        assert_eq!(s.steps[0].action, Action::Eval("echo hi".into()));
        assert_eq!(s.steps[0].expect.stdout.exact, Some(vec!["hi".into()]));
        assert_eq!(s.steps[0].expect.exit, ExitExpect::Code(0));
        assert_eq!(s.steps[1].expect.prompt, Some("Q?".into()));
        assert_eq!(s.steps[1].expect.choices, Some(vec!["a".into(), "b".into()]));
        assert_eq!(s.steps[2].action, Action::Answer("a".into()));
        assert_eq!(s.steps[3].action, Action::Abort);
        assert_eq!(s.steps[3].expect.exit, ExitExpect::Code(130));
    }

    #[test]
    fn payload_is_verbatim_after_single_space() {
        // Two spaces after `out` = one-space separator + one leading space of content:
        // significant whitespace (uu wc padding) is expressible.
        let s = parse_ok("run wc -l f\nout       3 f\n");
        assert_eq!(s.steps[0].expect.stdout.exact, Some(vec!["      3 f".into()]));
    }

    #[test]
    fn continuations_build_multiline_payloads() {
        let s = parse_ok("run cat <<EOF\n+ hello\n+ EOF\nout hello\n");
        assert_eq!(s.steps[0].action, Action::Eval("cat <<EOF\nhello\nEOF".into()));
    }

    #[test]
    fn continuation_after_expectation_is_rejected() {
        let e = parse_err("run echo hi\nout hi\n+ more\n");
        assert!(e.msg.contains("`+` continuation"), "{e}");
        assert_eq!(e.line, 3);
    }

    #[test]
    fn bare_out_means_one_empty_line() {
        let s = parse_ok("run echo\nout \n");
        assert_eq!(s.steps[0].expect.stdout.exact, Some(vec!["".into()]));
    }

    #[test]
    fn star_and_exact_conflict_is_rejected() {
        let e = parse_err("run echo hi\nout* \nout hi\n");
        assert!(e.msg.contains("cannot be combined"), "{e}");
    }

    #[test]
    fn expectation_before_any_step_is_rejected() {
        let e = parse_err("out hi\n");
        assert!(e.msg.contains("before any"), "{e}");
    }

    #[test]
    fn header_after_first_step_is_rejected() {
        let e = parse_err("run echo hi\n@only native\n");
        assert!(e.msg.contains("before the first step"), "{e}");
    }

    #[test]
    fn unknown_directive_is_rejected() {
        let e = parse_err("run echo hi\noutput hi\n");
        assert!(e.msg.contains("unknown directive `output`"), "{e}");
    }

    #[test]
    fn exit_duplicate_is_rejected_regardless_of_order() {
        // `exit 0` first used to be indistinguishable from the unset default, making
        // duplicate detection order-dependent.
        let e = parse_err("run false\nexit 0\nexit 1\n");
        assert!(e.msg.contains("stated twice"), "{e}");
        let e = parse_err("run false\nexit 1\nexit 0\n");
        assert!(e.msg.contains("stated twice"), "{e}");
        // A fresh step gets a fresh slate.
        parse_ok("run false\nexit 1\n\nrun true\nexit 0\n");
    }

    #[test]
    fn keyword_without_separator_is_rejected() {
        for text in ["run echo\nout\n", "run echo\nerr\n", "run p\nanswer\n"] {
            let e = parse_err(text);
            assert!(e.msg.contains("needs a payload"), "{text:?} → {e}");
        }
        // Bare `abort` legitimately has no payload.
        parse_ok("run prompt-user \"Q?\"\nout~ Q\nprompt~ Q\n\nabort\nexit 130\n");
    }

    #[test]
    fn only_duplicate_is_rejected() {
        let e = parse_err("@only native\n@only golem\nrun echo hi\nout hi\n");
        assert!(e.msg.contains("`@only` stated twice"), "{e}");
    }

    #[test]
    fn blank_line_closes_a_continuation_block() {
        // A blank line between `run` and `+` is a loud error, not a silently-dropped
        // heredoc content line; the blank content line is spelled as a lone `+`.
        let e = parse_err("run cat <<EOF\n\n+ hello\n+ EOF\n");
        assert!(e.msg.contains("`+` continuation"), "{e}");
        let s = parse_ok("run cat <<EOF\n+\n+ EOF\nout \n");
        assert_eq!(s.steps[0].action, Action::Eval("cat <<EOF\n\nEOF".into()));
    }

    #[test]
    fn choices_are_verbatim_between_commas() {
        let s = parse_ok("run p\nout* \nprompt~ Q\nchoices yes,no\n");
        assert_eq!(s.steps[0].expect.choices, Some(vec!["yes".into(), "no".into()]));
        // No trimming: a space after the comma is part of the item.
        let s = parse_ok("run p\nout* \nprompt~ Q\nchoices a, b\n");
        assert_eq!(s.steps[0].expect.choices, Some(vec!["a".into(), " b".into()]));
    }

    #[test]
    fn substitution_reaches_payloads_and_expectations() {
        let s = parse_ok("run cat ${TMP}/f\nout ${TMP} content\nprompt~ in ${TMP}?\n");
        let r = s.steps[0].resolved("/tmp/x");
        assert_eq!(r.action, Action::Eval("cat /tmp/x/f".into()));
        assert_eq!(r.expect.stdout.exact, Some(vec!["/tmp/x content".into()]));
        assert_eq!(r.expect.prompt, Some("in /tmp/x?".into()));
    }
}
