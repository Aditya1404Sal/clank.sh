//! The `model` builtin: list and configure model providers and the default model.
//!
//! `model` is an ordinary Brush [`SimpleCommand`] (like [`which`](crate::which)): it does no HTTP and
//! touches no `Session` state — only `std::fs` on `~/.config/ask/ask.toml` (see [`crate::askconfig`]).
//! That makes it free inside pipes and `$(...)` and keeps it out of the Session interception list. The
//! `ask` path re-reads ask.toml per invocation, so the file is the single source of truth (no cache).
//!
//! **Provider keys are never stored.** `model add --key …` reports an honest error pointing at
//! `ANTHROPIC_API_KEY` (the agent reads it from its environment via golem.yaml). The model catalog is
//! a hardcoded honest subset — the README's `model list` over a single provider (anthropic) — stated
//! as such in the help.

use std::io::Write;

use brush_core::builtins::{ContentOptions, ContentType, Registration, SimpleCommand};
use brush_core::commands::ExecutionContext;
use brush_core::extensions::ShellExtensions;
use brush_core::{Error, ExecutionResult};

use crate::manifest::{ExecutionScope, Manifest};

pub(crate) struct Model;

impl Model {
    pub(crate) const NAME: &'static str = "model";
    pub(crate) const SYNOPSIS: &'static str = "list and configure model providers and the default model";
}

/// The only provider clank speaks to today.
const PROVIDER: &str = "anthropic";

/// The built-in model catalog (an honest subset; the provider accepts any id, these are the vetted
/// ones). The first entry is the built-in fallback default (matches [`crate::askcmd::DEFAULT_MODEL`]).
const CATALOG: &[&str] = &[
    "anthropic/claude-opus-4-8",
    "anthropic/claude-sonnet-4-5",
    "anthropic/claude-haiku-4-5",
];

impl SimpleCommand for Model {
    fn get_content(
        name: &str,
        content_type: ContentType,
        _options: &ContentOptions,
    ) -> Result<String, Error> {
        match content_type {
            ContentType::ShortDescription => Ok(format!("{name} - {}\n", Model::SYNOPSIS)),
            ContentType::ShortUsage => Ok(format!(
                "{name}: {name} list | default <id> | info [<id>] | add <provider> [--key K] | remove <provider>\n"
            )),
            ContentType::DetailedHelp => Ok(format!(
                "{name} - {}\n\n\
                 Subcommands:\n\
                 \x20 model list              list the built-in model catalog (the default is marked *)\n\
                 \x20 model default <id>      set the default model (writes ~/.config/ask/ask.toml)\n\
                 \x20 model info [<id>]       show details for a model (or the current default)\n\
                 \x20 model add <provider>    (keys are read from the environment, not stored — see below)\n\
                 \x20 model remove <provider> remove a provider (anthropic is built in)\n\n\
                 Model ids use `provider/model`; the provider prefix may be omitted for unambiguous names.\n\
                 The catalog is a curated subset — the provider accepts other ids, passed through as-is.\n\
                 Provider API keys are NOT stored in ask.toml: set ANTHROPIC_API_KEY in the environment\n\
                 (the Golem agent receives it via golem.yaml).\n\n\
                 (clank builtin)\n",
                Model::SYNOPSIS
            )),
            ContentType::ManPage => brush_core::error::unimp("man page not yet implemented"),
        }
    }

    fn execute<SE, I, S>(context: ExecutionContext<'_, SE>, args: I) -> Result<ExecutionResult, Error>
    where
        SE: ShellExtensions,
        I: Iterator<Item = S>,
        S: AsRef<str>,
    {
        let argv: Vec<String> = args.skip(1).map(|s| s.as_ref().to_string()).collect();
        // Resolve $HOME the way `which` resolves $PATH (before borrowing stdout).
        let home = context
            .shell
            .env()
            .get_str("HOME", context.shell)
            .map(|h| h.into_owned())
            .unwrap_or_else(|| "/home/user".to_string());

        let (stdout, stderr, code) = run(&home, &argv);
        if !stdout.is_empty() {
            let _ = context.stdout().write_all(stdout.as_bytes());
        }
        if !stderr.is_empty() {
            let _ = context.stderr().write_all(stderr.as_bytes());
        }
        Ok(ExecutionResult::new(code))
    }
}

/// Run `model <argv>` against the ask.toml under `home`. Returns `(stdout, stderr, exit_code)` so the
/// logic is testable without an `ExecutionContext`.
fn run(home: &str, argv: &[String]) -> (String, String, u8) {
    let sub = argv.first().map(String::as_str).unwrap_or("list");
    match sub {
        "list" => list(home),
        "default" => set_default(home, argv.get(1).map(String::as_str)),
        "info" => info(home, argv.get(1).map(String::as_str)),
        "add" => add(argv),
        "remove" => remove(argv.get(1).map(String::as_str)),
        other => (
            String::new(),
            format!("model: unknown subcommand '{other}' (try: list, default, info, add, remove)\n"),
            2,
        ),
    }
}

/// The resolved default: the ask.toml value, else the built-in fallback. On a parse error, warn to
/// stderr and fall back. Returns `(default_id, source, warning)`.
fn resolved_default(home: &str) -> (String, &'static str, Option<String>) {
    match crate::askconfig::default_model(home) {
        Ok(Some(m)) => (m, "~/.config/ask/ask.toml", None),
        Ok(None) => (crate::askcmd::DEFAULT_MODEL.to_string(), "built-in", None),
        Err(e) => (
            crate::askcmd::DEFAULT_MODEL.to_string(),
            "built-in",
            Some(format!("model: {e}; using the built-in default\n")),
        ),
    }
}

fn list(home: &str) -> (String, String, u8) {
    let (default, source, warn) = resolved_default(home);
    let default_id = canonicalize(&default);
    let mut out = String::new();
    for id in CATALOG {
        let marker = if canonicalize(id) == default_id { "* " } else { "  " };
        out.push_str(&format!("{marker}{id}\n"));
    }
    out.push_str(&format!("\ndefault: {default} (from {source})\n"));
    (out, warn.unwrap_or_default(), 0)
}

fn set_default(home: &str, id: Option<&str>) -> (String, String, u8) {
    let Some(id) = id else {
        return (String::new(), "model default: needs a model id\n".into(), 2);
    };
    // Canonicalize a bare id to `anthropic/…`; reject an unknown provider prefix.
    let canon = match canonicalize_checked(id) {
        Ok(c) => c,
        Err(e) => return (String::new(), e, 2),
    };
    let mut warn = String::new();
    if !CATALOG.iter().any(|c| *c == canon) {
        warn = format!("model default: '{canon}' is not in the built-in catalog; passing it to the provider as-is\n");
    }
    match crate::askconfig::save_default_model(home, &canon) {
        Ok(()) => (format!("default model set to {canon}\n"), warn, 0),
        Err(e) => (String::new(), format!("model default: {e}\n"), 1),
    }
}

fn info(home: &str, id: Option<&str>) -> (String, String, u8) {
    let (default, _source, warn) = resolved_default(home);
    let target = id.map(canonicalize).unwrap_or_else(|| canonicalize(&default));
    let is_default = target == canonicalize(&default);
    let provider = target.split('/').next().unwrap_or(PROVIDER);
    let in_catalog = CATALOG.iter().any(|c| *c == target);
    let mut out = String::new();
    out.push_str(&format!("id:        {target}\n"));
    out.push_str(&format!("provider:  {provider}\n"));
    out.push_str(&format!("in catalog: {}\n", if in_catalog { "yes" } else { "no (passed through)" }));
    out.push_str(&format!("default:   {}\n", if is_default { "yes" } else { "no" }));
    (out, warn.unwrap_or_default(), 0)
}

fn add(argv: &[String]) -> (String, String, u8) {
    let provider = argv.get(1).map(String::as_str).unwrap_or("");
    // Never echo a key value, even on the error path.
    let msg = if provider == PROVIDER || provider.is_empty() {
        "model add: provider keys are not stored in ask.toml; set ANTHROPIC_API_KEY in the \
         environment (the Golem agent receives it via golem.yaml). anthropic is built in.\n"
            .to_string()
    } else {
        format!(
            "model add: only the built-in anthropic provider is supported; '{provider}' cannot be \
             added. Set ANTHROPIC_API_KEY in the environment.\n"
        )
    };
    (String::new(), msg, 1)
}

fn remove(provider: Option<&str>) -> (String, String, u8) {
    let p = provider.unwrap_or("");
    let msg = if p == PROVIDER || p.is_empty() {
        "model remove: anthropic is built in and cannot be removed.\n".to_string()
    } else {
        format!("model remove: no such provider '{p}' (only the built-in anthropic exists).\n")
    };
    (String::new(), msg, 1)
}

/// Add the `anthropic/` prefix to a bare id; leave a `provider/model` id unchanged.
fn canonicalize(id: &str) -> String {
    if id.contains('/') {
        id.to_string()
    } else {
        format!("{PROVIDER}/{id}")
    }
}

/// Like [`canonicalize`] but rejects an unknown provider prefix (only `anthropic/` is valid today).
fn canonicalize_checked(id: &str) -> Result<String, String> {
    if let Some((provider, _)) = id.split_once('/') {
        if provider != PROVIDER {
            return Err(format!(
                "model default: unknown provider '{provider}' (only {PROVIDER} is available)\n"
            ));
        }
        Ok(id.to_string())
    } else {
        Ok(format!("{PROVIDER}/{id}"))
    }
}

/// The `model` builtin registration, for `build_shell`.
pub(crate) fn builtins<SE: ShellExtensions>() -> Vec<(String, Registration<SE>)> {
    use brush_core::builtins::simple_builtin;
    vec![(Model::NAME.into(), simple_builtin::<Model, SE>())]
}

/// The `model` manifest. `Subprocess` scope (README command table), `Allow` (writes only the user's
/// own config; no network, not destructive).
pub(crate) fn manifests() -> Vec<Manifest> {
    vec![Manifest::builtin(Model::NAME, Model::SYNOPSIS).with_scope(ExecutionScope::Subprocess)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("clank_modelcmd_{}_{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_str().unwrap().to_string()
    }

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn list_marks_the_builtin_default() {
        let home = temp_home();
        let (out, _err, code) = run(&home, &args(&["list"]));
        assert_eq!(code, 0);
        // Fresh home: the built-in default (opus) is marked with *.
        assert!(out.contains("* anthropic/claude-opus-4-8"), "got: {out}");
        assert!(out.contains("(from built-in)"), "got: {out}");
    }

    #[test]
    fn default_sets_and_list_reflects_it() {
        let home = temp_home();
        let (out, _err, code) = run(&home, &args(&["default", "anthropic/claude-sonnet-4-5"]));
        assert_eq!(code, 0, "err path: {out}");
        assert!(out.contains("claude-sonnet-4-5"));
        let (list_out, _, _) = run(&home, &args(&["list"]));
        assert!(list_out.contains("* anthropic/claude-sonnet-4-5"), "got: {list_out}");
        assert!(list_out.contains("from ~/.config/ask/ask.toml"), "got: {list_out}");
    }

    #[test]
    fn default_canonicalizes_a_bare_id() {
        let home = temp_home();
        run(&home, &args(&["default", "claude-haiku-4-5"]));
        assert_eq!(
            crate::askconfig::default_model(&home).unwrap().as_deref(),
            Some("anthropic/claude-haiku-4-5")
        );
    }

    #[test]
    fn default_rejects_unknown_provider() {
        let home = temp_home();
        let (_out, err, code) = run(&home, &args(&["default", "openai/gpt-4o"]));
        assert_eq!(code, 2);
        assert!(err.contains("unknown provider 'openai'"), "got: {err}");
    }

    #[test]
    fn default_warns_off_catalog_but_saves() {
        let home = temp_home();
        let (_out, err, code) = run(&home, &args(&["default", "anthropic/claude-future-9"]));
        assert_eq!(code, 0);
        assert!(err.contains("not in the built-in catalog"), "got: {err}");
        assert_eq!(
            crate::askconfig::default_model(&home).unwrap().as_deref(),
            Some("anthropic/claude-future-9")
        );
    }

    #[test]
    fn add_is_an_honest_error_naming_the_env_var() {
        let home = temp_home();
        let (out, err, code) = run(&home, &args(&["add", "anthropic", "--key", "sk-secret-value"]));
        assert_eq!(code, 1);
        assert!(out.is_empty());
        assert!(err.contains("ANTHROPIC_API_KEY"), "got: {err}");
        // The key value must never be echoed.
        assert!(!err.contains("sk-secret-value"), "key leaked: {err}");
    }

    #[test]
    fn info_reports_default_status() {
        let home = temp_home();
        run(&home, &args(&["default", "anthropic/claude-sonnet-4-5"]));
        let (out, _err, code) = run(&home, &args(&["info", "claude-sonnet-4-5"]));
        assert_eq!(code, 0);
        assert!(out.contains("provider:  anthropic"), "got: {out}");
        assert!(out.contains("default:   yes"), "got: {out}");
    }

    #[test]
    fn unknown_subcommand_errors() {
        let home = temp_home();
        let (_out, err, code) = run(&home, &args(&["frobnicate"]));
        assert_eq!(code, 2);
        assert!(err.contains("unknown subcommand"));
    }
}
