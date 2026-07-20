//! The `ps` builtin: reports the process table.
//!
//! Hand-written (not the `uu_builtin!` macro) because there is no uutils `uumain` for it and no fd
//! swap is needed — it writes its rendered table straight to `context.stdout()` (an `impl Write`),
//! so it composes as a pipeline source (`ps | grep …`) without the stdin-capture limitation the
//! uu_* builtins have.
//!
//! It reads the process table via [`proctable::active`] — the table `Session::run_line` installs for
//! the duration of the current line. Because `ps` runs *inside* a `run_line`, its own row is present
//! and still `R` (the row is marked `Z` only after `execute` returns), exactly like real Unix.

use std::io::Write;

use brush_core::builtins::{ContentOptions, ContentType, Registration, SimpleCommand};
use brush_core::commands::ExecutionContext;
use brush_core::extensions::ShellExtensions;
use brush_core::{Error, ExecutionResult};

use crate::manifest::Manifest;
use crate::runtime::proctable::{self, PsMode};

pub(crate) struct Ps;

impl Ps {
    const NAME: &'static str = "ps";
    const SYNOPSIS: &'static str = "report process status";
}

/// Pick the output mode from the flags. Lenient — unknown flags are ignored (like clank's other
/// builtins). `aux` (BSD, no dash) and `-ef`/`-e`/`-f` (System V) are recognized.
fn parse_mode<S: AsRef<str>>(args: &[S]) -> PsMode {
    let mut aux = false;
    let mut ef = false;
    // Skip argv[0] (the command name Brush passes).
    for a in args.iter().skip(1) {
        match a.as_ref() {
            "aux" => aux = true,
            s if s.starts_with('-') && (s.contains('e') || s.contains('f')) => ef = true,
            _ => {}
        }
    }
    if aux {
        PsMode::Aux
    } else if ef {
        PsMode::Ef
    } else {
        PsMode::Default
    }
}

impl SimpleCommand for Ps {
    fn get_content(
        name: &str,
        content_type: ContentType,
        _options: &ContentOptions,
    ) -> Result<String, Error> {
        match content_type {
            ContentType::ShortDescription => Ok(format!("{name} - {}\n", Ps::SYNOPSIS)),
            ContentType::ShortUsage => Ok(format!("{name}: {name} [aux|-ef]\n")),
            ContentType::DetailedHelp => {
                Ok(format!("{name} - {}\n\n(clank builtin)\n", Ps::SYNOPSIS))
            }
            ContentType::ManPage => brush_core::error::unimp("man page not yet implemented"),
        }
    }

    fn execute<SE, I, S>(context: ExecutionContext<'_, SE>, args: I) -> Result<ExecutionResult, Error>
    where
        SE: ShellExtensions,
        I: Iterator<Item = S>,
        S: AsRef<str>,
    {
        let argv: Vec<String> = args.map(|s| s.as_ref().to_string()).collect();
        let mode = parse_mode(&argv);

        // Render from the table installed for the current line. If none is installed (ps invoked
        // outside a run_line — shouldn't happen in practice), render an empty table with just the
        // synthetic root.
        let rendered = match proctable::active() {
            Some(table) => table.lock().unwrap().render_ps(mode),
            None => proctable::ProcessTable::new().render_ps(mode),
        };

        let mut out = context.stdout();
        let _ = out.write_all(rendered.as_bytes());
        Ok(ExecutionResult::new(0))
    }
}

/// The `ps` builtin registration, for `build_shell`.
pub(crate) fn builtins<SE: ShellExtensions>() -> Vec<(String, Registration<SE>)> {
    use crate::builtins::helpshim::simple_builtin_with_help;
    vec![("ps".into(), simple_builtin_with_help::<Ps, SE>())]
}

/// The `ps` manifest, for the command registry. `ps` is `Subprocess` scope (README), `Allow`.
pub(crate) fn manifests() -> Vec<Manifest> {
    vec![Manifest::builtin(Ps::NAME, Ps::SYNOPSIS)]
}
