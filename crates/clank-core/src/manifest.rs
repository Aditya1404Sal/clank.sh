//! Command manifests: the typed metadata every clank-resolvable command carries.
//!
//! A manifest is **static** data describing a *command* (a name that can be resolved) — it exists
//! whether or not the command is running, and is distinct from a *process* (a running invocation,
//! added in a later increment) which will merely *reference* the manifest of the command it runs.
//!
//! This is the single artifact the README's process model builds on: it drives `type`/`which`/`man`,
//! `ps`/`/proc`, tab completion, authorization policy, and the tool surface the future `ask` exposes
//! to a model. Manifests are owned entirely by clank (no Brush dependency) so they can carry typed
//! fields — `execution_scope`, `authorization_policy` — that Brush's string-only `get_content`
//! surface cannot.
//!
//! `authorization_policy` is enforced at the pre-Brush authz gate (`crate::authz`, per top-level
//! command in a compound line); `execution_scope` is enforced at exactly one point — the `ask`
//! model-tool boundary (`ShellInternal`/`ParentShell` commands are refused as model tools, since a
//! subprocess-like tool can't reach parent-shell state); `redaction_rules` drive prompt-user
//! redaction. `subcommands` is populated for `mcp`/`grease`/`golem` (subcommand-aware gating).

/// What session state a command may touch — the README's three execution scopes. Since clank has no
/// OS process model (wasip2 can't spawn; native runs in-process too), "subprocess" means *isolated
/// from parent-shell state*, not a real fork. This is a classification, not a dispatch router (lines
/// are routed by interception pattern, not by reading this field). Its one production use is the
/// `ask` model-tool gate: only `Subprocess` commands may be called by the model — a `ShellInternal`/
/// `ParentShell` command would mutate state the isolated tool can't reach (see `session::run_ask`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutionScope {
    /// Runs in the parent shell context and mutates shell state; cannot be overridden.
    /// POSIX special builtins: `cd`, `exec`, `exit`, `export`, `source`, `unset`.
    ParentShell,
    /// Implemented in the shell, operating on shell-internal tables (jobs, aliases, transcript);
    /// cannot run as a subprocess. E.g. `alias`, `context`, `history`, `jobs`, `type`, `which`.
    ShellInternal,
    /// Runs as a subprocess with no access to parent shell state. E.g. `ls`, `grep`, `jq`, `ask`,
    /// installed scripts/prompts/agent executables.
    Subprocess,
}

/// The authorization policy required to invoke a command. Enforced at the pre-Brush authz gate
/// (`crate::authz::decide`), per top-level command in a compound line, for both a human line and the
/// model's per-tool-call gate. `sudo` (or a session-wide "all", for `Confirm` only) pre-authorizes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthorizationPolicy {
    /// May be invoked freely.
    Allow,
    /// Invocation pauses for user confirmation.
    Confirm,
    /// Only explicitly `sudo`-authorized invocations are permitted.
    SudoOnly,
}

/// The type of a single command parameter. The smallest honest typed schema — deliberately not
/// JSON Schema — sufficient for the README's "typed params" and cheap to construct.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParamType {
    String,
    Int,
    Path,
    /// A boolean switch (present/absent), e.g. `-r`.
    Flag,
    /// One of a fixed set of allowed string values.
    Enum(Vec<String>),
}

/// One typed parameter in a command's input schema.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParamSpec {
    pub name: String,
    pub ty: ParamType,
    pub required: bool,
    pub default: Option<String>,
}

/// Optional description of a command's structured output. A thin placeholder for now — the README
/// marks output-schema optional and we don't yet model structured output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputSchema {
    pub description: String,
}

/// The typed metadata for one command. Hierarchical: `subcommands` holds nested manifests
/// (recursively), though no clank builtin uses them yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    /// Kebab-case command name.
    pub name: String,
    /// One-line description.
    pub synopsis: String,
    pub execution_scope: ExecutionScope,
    /// Nested manifests, one per subcommand (empty for all current builtins).
    pub subcommands: Vec<Manifest>,
    /// Typed parameter definitions.
    pub input_schema: Vec<ParamSpec>,
    /// Optional typed description of structured output.
    pub output_schema: Option<OutputSchema>,
    pub authorization_policy: AuthorizationPolicy,
    /// Parameter names that must never appear in `ps`, logs, history, transcript, completion
    /// caches, or provider manifests. Empty for all current builtins.
    pub redaction_rules: Vec<String>,
    /// Full help content.
    pub help_text: String,
}

impl Manifest {
    /// A minimal manifest for a subprocess-scoped core command: `Subprocess` scope, `Allow`
    /// policy, no params/subcommands/redactions, help text defaulting to the synopsis. Keeps the
    /// ~23 builtin registration sites terse; callers refine fields as needed via the setters below.
    pub fn builtin(name: impl Into<String>, synopsis: impl Into<String>) -> Self {
        let name = name.into();
        let synopsis = synopsis.into();
        let help_text = synopsis.clone();
        Self {
            name,
            synopsis,
            execution_scope: ExecutionScope::Subprocess,
            subcommands: Vec::new(),
            input_schema: Vec::new(),
            output_schema: None,
            authorization_policy: AuthorizationPolicy::Allow,
            redaction_rules: Vec::new(),
            help_text,
        }
    }

    /// Override the execution scope (builder-style).
    #[must_use]
    pub fn with_scope(mut self, scope: ExecutionScope) -> Self {
        self.execution_scope = scope;
        self
    }

    /// Override the authorization policy (builder-style).
    #[must_use]
    pub fn with_policy(mut self, policy: AuthorizationPolicy) -> Self {
        self.authorization_policy = policy;
        self
    }

    /// Attach the input parameter schema (builder-style).
    #[must_use]
    pub fn with_params(mut self, params: Vec<ParamSpec>) -> Self {
        self.input_schema = params;
        self
    }

    /// Replace the help text (defaults to the synopsis) with full help content (builder-style).
    pub fn with_help(mut self, help_text: impl Into<String>) -> Self {
        self.help_text = help_text.into();
        self
    }

    /// Declare the parameter names whose values must be redacted from every rendered surface
    /// (`ps`, logs, history, transcript, completion caches, provider manifests) — builder-style.
    /// See [`redaction_rules`](Self::redaction_rules).
    #[must_use]
    pub fn with_redaction(mut self, rules: Vec<String>) -> Self {
        self.redaction_rules = rules;
        self
    }
}

/// Whether any word in `words` matches a declared `redaction-rules` entry — i.e. whether this
/// invocation is marked for redaction by the command's manifest (README:200/323). Pure and
/// unit-testable; the [`Session`](crate::session::Session) calls it to decide, from the manifest
/// rather than a hardcoded flag name, whether a `prompt-user` response (or any future redaction-ruled
/// command's marked value) must be kept out of the transcript/logs. Empty rules ⇒ never redacts.
#[must_use]
pub fn flags_trigger_redaction(rules: &[String], words: &[String]) -> bool {
    !rules.is_empty() && words.iter().any(|w| rules.iter().any(|r| r == w))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_constructor_sets_sensible_defaults() {
        let m = Manifest::builtin("ls", "list directory contents");
        assert_eq!(m.name, "ls");
        assert_eq!(m.synopsis, "list directory contents");
        assert_eq!(m.execution_scope, ExecutionScope::Subprocess);
        assert_eq!(m.authorization_policy, AuthorizationPolicy::Allow);
        assert!(m.subcommands.is_empty());
        assert!(m.input_schema.is_empty());
        assert!(m.redaction_rules.is_empty());
        // help_text defaults to the synopsis.
        assert_eq!(m.help_text, "list directory contents");
    }

    #[test]
    fn flags_trigger_redaction_is_manifest_driven() {
        let w = |s: &str| s.split_whitespace().map(String::from).collect::<Vec<_>>();
        // The built-in prompt-user rule.
        let rules = vec!["--secret".to_string()];
        assert!(flags_trigger_redaction(&rules, &w("prompt-user \"q\" --secret")));
        assert!(!flags_trigger_redaction(&rules, &w("prompt-user \"q\"")));
        // Empty rules never redact — even if the line carries a --secret-looking flag.
        assert!(!flags_trigger_redaction(&[], &w("prompt-user \"q\" --secret")));
        // The manifest is authoritative: a different declared rule drives a different trigger flag.
        let custom = vec!["--password".to_string()];
        assert!(flags_trigger_redaction(&custom, &w("login --password")));
        assert!(!flags_trigger_redaction(&custom, &w("login --secret")));
    }

    #[test]
    fn builder_setters_override_fields() {
        let m = Manifest::builtin("context", "manage the transcript")
            .with_scope(ExecutionScope::ShellInternal)
            .with_policy(AuthorizationPolicy::Confirm)
            .with_help("context show|clear|budget — manage the session transcript");
        assert_eq!(m.execution_scope, ExecutionScope::ShellInternal);
        assert_eq!(m.authorization_policy, AuthorizationPolicy::Confirm);
        assert!(m.help_text.contains("session transcript"));
    }

    #[test]
    fn params_and_nested_subcommands_round_trip() {
        let param = ParamSpec {
            name: "length".into(),
            ty: ParamType::Enum(vec!["short".into(), "long".into()]),
            required: false,
            default: Some("short".into()),
        };
        let child = Manifest::builtin("session", "manage sessions");
        let mut parent = Manifest::builtin("mcp", "manage MCP").with_params(vec![param.clone()]);
        parent.subcommands.push(child.clone());

        assert_eq!(parent.input_schema, vec![param]);
        assert_eq!(parent.subcommands.len(), 1);
        assert_eq!(parent.subcommands[0], child);
        // Clone is a deep clone.
        assert_eq!(parent.clone(), parent);
    }
}
