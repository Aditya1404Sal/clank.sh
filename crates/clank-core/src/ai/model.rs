//! The `model` builtin: list and configure model providers and the default model.
//!
//! `model` is an ordinary Brush [`SimpleCommand`] (like [`which`](crate::tools::which)): it does no HTTP and
//! touches no `Session` state — only `std::fs` on `~/.config/ask/ask.toml` (see [`crate::ai::config`]).
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
/// ones). Haiku is the built-in default (matches [`crate::ai::ask::DEFAULT_MODEL`]) — the lightest and
/// cheapest; opt into a bigger model explicitly.
const CATALOG: &[&str] = &[
    "anthropic/claude-haiku-4-5-20251001",
    "anthropic/claude-sonnet-4-5",
    "anthropic/claude-opus-4-8",
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
                 \x20 model add <provider> [--key K]  store a native key, or point at the env var\n\
                 \x20 model remove <provider> clear a stored provider key (native); anthropic stays built in\n\n\
                 Model ids use `provider/model`; the provider prefix may be omitted for unambiguous names.\n\
                 The catalog is a curated subset — the provider accepts other ids, passed through as-is.\n\
                 API keys: on native, `model add anthropic --key <k>` stores the key in ~/.config/ask/ask.toml;\n\
                 without --key (and on the Golem agent), set ANTHROPIC_API_KEY in the environment.\n\n\
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
            .map(std::borrow::Cow::into_owned)
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
    let sub = argv.first().map_or("list", String::as_str);
    match sub {
        "list" => list(home),
        "default" => set_default(home, argv.get(1).map(String::as_str)),
        "info" => info(home, argv.get(1).map(String::as_str)),
        "add" => add(home, argv),
        "remove" => remove(home, argv.get(1).map(String::as_str)),
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
    match crate::ai::config::default_model(home) {
        Ok(Some(m)) => (m, "~/.config/ask/ask.toml", None),
        Ok(None) => (crate::ai::ask::DEFAULT_MODEL.to_string(), "built-in", None),
        Err(e) => (
            crate::ai::ask::DEFAULT_MODEL.to_string(),
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
    match crate::ai::config::save_default_model(home, &canon) {
        Ok(()) => (format!("default model set to {canon}\n"), warn, 0),
        Err(e) => (String::new(), format!("model default: {e}\n"), 1),
    }
}

fn info(home: &str, id: Option<&str>) -> (String, String, u8) {
    let (default, _source, warn) = resolved_default(home);
    let target = id.map_or_else(|| canonicalize(&default), canonicalize);
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

/// `model add <provider> [--key <k>]`. Only the built-in `anthropic` provider is known.
///
/// On **native**, `--key <k>` persists the key to `~/.config/ask/ask.toml` `[providers.anthropic].key`
/// (README §444) so the native `ask` provider can read it; without `--key` it points at the env var.
/// On the **agent** (wasm), keys are never written to the durable FS — the message directs the user to
/// `ANTHROPIC_API_KEY` (golem.yaml passthrough), matching the durable provider's `from_env` contract.
fn add(home: &str, argv: &[String]) -> (String, String, u8) {
    let _ = home; // used only on native (see below)
    let provider = argv.get(1).map_or("", String::as_str);
    if !(provider == PROVIDER || provider.is_empty()) {
        return (
            String::new(),
            format!(
                "model add: only the built-in anthropic provider is supported; '{provider}' cannot \
                 be added.\n"
            ),
            1,
        );
    }

    // Extract a `--key <value>` if present. Never echo the value on any path.
    let key = argv
        .iter()
        .position(|a| a == "--key")
        .and_then(|i| argv.get(i + 1))
        .map(String::as_str);

    #[cfg(not(target_arch = "wasm32"))]
    {
        match key {
            Some(k) if !k.is_empty() => match crate::ai::config::save_provider_key(home, PROVIDER, k) {
                Ok(()) => (
                    "model add: stored the anthropic key in ~/.config/ask/ask.toml\n".to_string(),
                    String::new(),
                    0,
                ),
                Err(e) => (String::new(), format!("model add: {e}\n"), 1),
            },
            _ => (
                String::new(),
                "model add: anthropic is built in. Provide a key with `--key <value>` to store it \
                 in ~/.config/ask/ask.toml, or set ANTHROPIC_API_KEY in the environment.\n"
                    .to_string(),
                1,
            ),
        }
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = key;
        (
            String::new(),
            "model add: provider keys are not stored on the agent; set ANTHROPIC_API_KEY in the \
             environment (the Golem agent receives it via golem.yaml). anthropic is built in.\n"
                .to_string(),
            1,
        )
    }
}

/// `model remove <provider>` clears a stored provider key. anthropic is built in and cannot be
/// removed as a provider, but its stored key (from `model add anthropic --key …`) CAN be cleared,
/// reverting auth to `ANTHROPIC_API_KEY` — the useful, honest meaning of "remove" here. On the agent,
/// keys are never stored, so it's an honest no-op message.
fn remove(home: &str, provider: Option<&str>) -> (String, String, u8) {
    let _ = home; // used only on native
    let p = provider.unwrap_or("");
    if !(p == PROVIDER || p.is_empty()) {
        return (
            String::new(),
            format!("model remove: no such provider '{p}' (only the built-in anthropic exists).\n"),
            1,
        );
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        match crate::ai::config::remove_provider_key(home, PROVIDER) {
            Ok(true) => (
                "model remove: cleared the stored anthropic key from ~/.config/ask/ask.toml; auth \
                 now falls back to ANTHROPIC_API_KEY. anthropic stays built in.\n"
                    .to_string(),
                String::new(),
                0,
            ),
            Ok(false) => (
                String::new(),
                "model remove: no stored anthropic key to clear (anthropic is built in and reads \
                 ANTHROPIC_API_KEY).\n"
                    .to_string(),
                1,
            ),
            Err(e) => (String::new(), format!("model remove: {e}\n"), 1),
        }
    }
    #[cfg(target_arch = "wasm32")]
    {
        (
            String::new(),
            "model remove: provider keys are not stored on the agent (anthropic reads \
             ANTHROPIC_API_KEY via golem.yaml). anthropic is built in.\n"
                .to_string(),
            1,
        )
    }
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
    use crate::builtins::helpshim::simple_builtin_with_help;
    vec![(Model::NAME.into(), simple_builtin_with_help::<Model, SE>())]
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
        parts.iter().map(std::string::ToString::to_string).collect()
    }

    #[test]
    fn list_marks_the_builtin_default() {
        let home = temp_home();
        let (out, _err, code) = run(&home, &args(&["list"]));
        assert_eq!(code, 0);
        // Fresh home: the built-in default (haiku) is marked with *.
        assert!(out.contains("* anthropic/claude-haiku-4-5-20251001"), "got: {out}");
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
            crate::ai::config::default_model(&home).unwrap().as_deref(),
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
            crate::ai::config::default_model(&home).unwrap().as_deref(),
            Some("anthropic/claude-future-9")
        );
    }

    #[test]
    fn add_with_key_stores_it_in_ask_toml() {
        let home = temp_home();
        let (out, err, code) = run(&home, &args(&["add", "anthropic", "--key", "sk-secret-value"]));
        assert_eq!(code, 0, "native add --key should succeed; err: {err}");
        assert!(out.contains("ask.toml"), "got: {out}");
        // The key value must never be echoed on any channel.
        assert!(!out.contains("sk-secret-value") && !err.contains("sk-secret-value"), "key leaked");
        // It round-trips through the config layer.
        assert_eq!(
            crate::ai::config::provider_key(&home, "anthropic").as_deref(),
            Some("sk-secret-value")
        );
    }

    #[test]
    fn add_without_key_points_at_config_or_env() {
        let home = temp_home();
        let (out, err, code) = run(&home, &args(&["add", "anthropic"]));
        assert_eq!(code, 1);
        assert!(out.is_empty());
        // Directs the user to --key or the env var; never stores anything.
        assert!(err.contains("--key") || err.contains("ANTHROPIC_API_KEY"), "got: {err}");
    }

    #[test]
    fn add_unknown_provider_is_rejected() {
        let home = temp_home();
        let (_out, err, code) = run(&home, &args(&["add", "openai", "--key", "x"]));
        assert_eq!(code, 1);
        assert!(err.contains("only the built-in anthropic"), "got: {err}");
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
