//! The `grease` command: classifier + grammar for the package manager.
//!
//! `grease` is a **Session-layer interception** (like `mcp`/`ask`): its `install`/`update`/`search`
//! subcommands do outbound HTTP, which must await under the live WASI-HTTP reactor. The work lives in
//! `Session` methods; this module only parses. Mirrors [`crate::mcpcmd`].
//!
//! grease v1 grammar (prompts only):
//! ```text
//! grease registry add <url> | list | remove <url>
//! grease install <name>
//! grease remove <name>
//! grease list
//! grease search <query>
//! grease info <name>
//! grease update [<name>]
//! ```

use brush_parser::{tokenize_str, unquote_str, Token};

/// A parsed `grease` command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GreaseCommand {
    RegistryAdd { url: String, key: Option<String> },
    RegistryList,
    RegistryRemove { url: String },
    Install { name: String },
    Remove { name: String },
    List,
    Search { query: String },
    Info { name: String },
    Update { name: Option<String> },
}

/// Recognize a `grease` line. `None` when it isn't one (fall through to Brush); `Some(Err)` when it is
/// but doesn't parse. Lines with shell operators fall through so the nested-context stub handles them.
pub(crate) fn classify(line: &str) -> Option<Result<GreaseCommand, String>> {
    let tokens = tokenize_str(line).ok()?;
    if tokens.iter().any(|t| matches!(t, Token::Operator(_, _))) {
        return None;
    }
    let words: Vec<String> = tokens
        .into_iter()
        .filter_map(|t| match t {
            Token::Word(s, _) => Some(unquote_str(&s)),
            Token::Operator(_, _) => None,
        })
        .collect();
    if words.first().map(String::as_str) != Some("grease") {
        return None;
    }
    Some(parse(&words[1..]))
}

fn parse(args: &[String]) -> Result<GreaseCommand, String> {
    let sub = args.first().map(String::as_str).unwrap_or("list");
    match sub {
        "registry" => parse_registry(&args[1..]),
        "install" | "add" => {
            let name = args.get(1).ok_or("grease install: needs a package name")?;
            Ok(GreaseCommand::Install { name: name.clone() })
        }
        "remove" | "rm" | "uninstall" => {
            let name = args.get(1).ok_or("grease remove: needs a package name")?;
            Ok(GreaseCommand::Remove { name: name.clone() })
        }
        "list" | "ls" => Ok(GreaseCommand::List),
        "search" => {
            let query = args.get(1).ok_or("grease search: needs a query")?;
            Ok(GreaseCommand::Search { query: query.clone() })
        }
        "info" => {
            let name = args.get(1).ok_or("grease info: needs a package name")?;
            Ok(GreaseCommand::Info { name: name.clone() })
        }
        "update" | "upgrade" => Ok(GreaseCommand::Update { name: args.get(1).cloned() }),
        other => Err(format!(
            "grease: unknown subcommand '{other}' \
             (try: registry, install, remove, list, search, info, update)"
        )),
    }
}

fn parse_registry(args: &[String]) -> Result<GreaseCommand, String> {
    match args.first().map(String::as_str) {
        Some("list") | None => Ok(GreaseCommand::RegistryList),
        Some("add") => {
            // `grease registry add <url> [--key <base64-ed25519-pubkey>]`.
            let rest = &args[1..];
            let mut url: Option<String> = None;
            let mut key: Option<String> = None;
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                if a == "--key" {
                    key = Some(it.next().ok_or("grease registry add: --key needs a value")?.clone());
                } else if url.is_none() {
                    url = Some(a.clone());
                } else {
                    return Err(format!("grease registry add: unexpected argument '{a}'"));
                }
            }
            let url = url.ok_or("grease registry add: needs a <url>")?;
            Ok(GreaseCommand::RegistryAdd { url, key })
        }
        Some("remove") | Some("rm") => {
            let url = args.get(1).ok_or("grease registry remove: needs a <url>")?;
            Ok(GreaseCommand::RegistryRemove { url: url.clone() })
        }
        Some(other) => Err(format!(
            "grease registry: unknown subcommand '{other}' (try: add, list, remove)"
        )),
    }
}

/// The static `grease` manifest with per-subcommand authorization policy. The authz gate is
/// subcommand-aware ([`crate::authz::resolve`]), so the top level is `Allow` and the mutating
/// subcommands (`install`/`remove`/`update`/`registry`) carry `Confirm` (network + filesystem writes).
pub(crate) fn manifests() -> Vec<crate::manifest::Manifest> {
    use crate::manifest::{AuthorizationPolicy, ExecutionScope, Manifest};
    let confirm = |name: &str, synopsis: &str| {
        Manifest::builtin(name, synopsis)
            .with_scope(ExecutionScope::Subprocess)
            .with_policy(AuthorizationPolicy::Confirm)
    };
    let allow = |name: &str, synopsis: &str| {
        Manifest::builtin(name, synopsis).with_scope(ExecutionScope::Subprocess)
    };
    let mut m = Manifest::builtin("grease", "install and manage capability packages")
        .with_scope(ExecutionScope::Subprocess)
        .with_help(
            "grease registry add <url> [--key <ed25519-pubkey>] | list | remove <url> — registries\n\
             grease install <name> — install a package (outbound HTTP); the registry declares its kind\n\
             grease remove <name> — uninstall a package\n\
             grease list — installed packages\n\
             grease search <query> — search the registries (outbound HTTP)\n\
             grease info <name> — show a package's metadata\n\
             grease update [<name>] — re-fetch installed packages (outbound HTTP)\n\
             Package kinds: prompt (runs via ask), script (/usr/bin, runs local shell), \
             skill (/usr/share/skills, model context + $PATH scripts).",
        );
    m.subcommands = vec![
        // `registry` is Allow at this level (its `list` is read-only; `add`/`remove` write only local
        // config, no network — the same low-risk local-config tier as mcp's `session`). The outbound
        // HTTP + payload-installing subcommands below carry Confirm.
        allow("registry", "manage package registries"),
        confirm("install", "install a package (outbound HTTP)"),
        confirm("remove", "uninstall a package"),
        confirm("update", "re-fetch installed packages (outbound HTTP)"),
        allow("list", "list installed packages"),
        allow("search", "search the registries (outbound HTTP)"),
        allow("info", "show a package's metadata"),
    ];
    vec![m]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(line: &str) -> GreaseCommand {
        classify(line).unwrap().unwrap()
    }

    #[test]
    fn classifies_registry_subcommands() {
        assert_eq!(
            c("grease registry add https://reg.example"),
            GreaseCommand::RegistryAdd { url: "https://reg.example".into(), key: None }
        );
        // `--key` attaches a trusted signing key.
        assert_eq!(
            c("grease registry add https://reg.example --key AAAA"),
            GreaseCommand::RegistryAdd { url: "https://reg.example".into(), key: Some("AAAA".into()) }
        );
        assert_eq!(c("grease registry list"), GreaseCommand::RegistryList);
        assert_eq!(c("grease registry"), GreaseCommand::RegistryList); // bare = list
        assert_eq!(
            c("grease registry remove https://reg.example"),
            GreaseCommand::RegistryRemove { url: "https://reg.example".into() }
        );
        // `--key` without a value is an error.
        assert!(classify("grease registry add https://r --key").unwrap().is_err());
    }

    #[test]
    fn classifies_package_subcommands() {
        assert_eq!(c("grease install summarize"), GreaseCommand::Install { name: "summarize".into() });
        assert_eq!(c("grease remove summarize"), GreaseCommand::Remove { name: "summarize".into() });
        assert_eq!(c("grease list"), GreaseCommand::List);
        assert_eq!(c("grease"), GreaseCommand::List); // bare grease = list
        assert_eq!(c("grease search review"), GreaseCommand::Search { query: "review".into() });
        assert_eq!(c("grease info summarize"), GreaseCommand::Info { name: "summarize".into() });
        assert_eq!(c("grease update"), GreaseCommand::Update { name: None });
        assert_eq!(c("grease update summarize"), GreaseCommand::Update { name: Some("summarize".into()) });
    }

    #[test]
    fn non_grease_and_operator_lines_are_none() {
        assert!(classify("echo grease").is_none());
        assert!(classify("cat file").is_none());
        // A line with an operator falls through (nested-context stub handles it).
        assert!(classify("grease list | head").is_none());
        assert!(classify("grease list && echo done").is_none());
    }

    #[test]
    fn unknown_subcommands_error() {
        assert!(classify("grease frobnicate").unwrap().is_err());
        assert!(classify("grease registry frob").unwrap().is_err());
        assert!(classify("grease install").unwrap().is_err()); // missing name
        assert!(classify("grease registry add").unwrap().is_err()); // missing url
    }

    #[test]
    fn manifest_has_subcommand_policies() {
        use crate::manifest::AuthorizationPolicy;
        let m = &manifests()[0];
        assert_eq!(m.name, "grease");
        let policy = |sub: &str| {
            m.subcommands.iter().find(|s| s.name == sub).unwrap().authorization_policy
        };
        assert_eq!(policy("install"), AuthorizationPolicy::Confirm);
        assert_eq!(policy("remove"), AuthorizationPolicy::Confirm);
        assert_eq!(policy("update"), AuthorizationPolicy::Confirm);
        assert_eq!(policy("registry"), AuthorizationPolicy::Allow);
        assert_eq!(policy("list"), AuthorizationPolicy::Allow);
        assert_eq!(policy("info"), AuthorizationPolicy::Allow);
    }
}
