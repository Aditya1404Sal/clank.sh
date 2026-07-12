//! `grease` package types: the registry payloads, `{{var}}` templating, and sha256 integrity.
//!
//! grease installs several kinds of package (see [`PackageKind`]); this module models the ones this
//! build supports:
//!
//! - **Prompt** ([`PromptPackage`]) — `{name, description, model?, arguments?, body}`; the `body`
//!   (after `{{var}}` substitution) is what a prompt invocation passes to `ask`.
//! - **Script** ([`ScriptPackage`]) — `{name, description, arguments?, body}`; the `body` is shell
//!   source run through the Brush engine (`run_string`) as a synthetic in-component process.
//! - **Skill** ([`SkillPackage`]) — a capability-context envelope `{name, description, intended-use?,
//!   documents?, scripts?}`; not a command, surfaced to the model + its bundled scripts land on `$PATH`.
//!
//! Prompt and script arguments share the [`PackageArg`] shape and the `{{var}}` [`fill`] machinery.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::manifest::{ParamSpec, ParamType};

/// Which kind of package a registry entry / install marker describes. Declared by the registry
/// (index `kind` field) and recorded in the install marker so `load` can dispatch to the right
/// payload parser. Defaults to `Prompt` (kind-less markers written by grease v1 still load).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PackageKind {
    #[default]
    Prompt,
    Script,
    Skill,
    /// An MCP server's artifacts (tools/prompts/resources) — the server name becomes a command whose
    /// tools are subcommands (README:657).
    Mcp,
    /// A Golem agent type — becomes a `/usr/lib/agents/bin/<name>` command that invokes the agent via
    /// wRPC (README:671).
    Agent,
}

impl PackageKind {
    /// The payload filename this kind is stored under in `<store>/<name>/`.
    pub fn payload_file(self) -> &'static str {
        match self {
            PackageKind::Prompt => "prompt.json",
            PackageKind::Script => "script.json",
            PackageKind::Skill => "skill.json",
            PackageKind::Mcp => "mcp.json",
            PackageKind::Agent => "agent.json",
        }
    }

    /// A short human label (for `grease list`/`info`).
    pub fn label(self) -> &'static str {
        match self {
            PackageKind::Prompt => "prompt",
            PackageKind::Script => "script",
            PackageKind::Skill => "skill",
            PackageKind::Mcp => "mcp",
            PackageKind::Agent => "agent",
        }
    }
}

/// One declared argument, shared by prompt and script packages (both template a `body` via `{{var}}`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct PackageArg {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

/// Back-compat alias — prompt code (and its tests) refer to `PromptArg`.
pub type PromptArg = PackageArg;

/// The declared arguments rendered as manifest [`ParamSpec`]s (drives `--help`, completion). All are
/// `String`-typed (free text); `required`/`default` carry through. Shared by prompt and script.
fn args_to_param_specs(args: &[PackageArg]) -> Vec<ParamSpec> {
    args.iter()
        .map(|a| ParamSpec {
            name: a.name.clone(),
            ty: ParamType::String,
            required: a.required,
            default: a.default.clone(),
        })
        .collect()
}

/// Fill `body`'s `{{arg}}` placeholders from `provided` (arg name → value). A declared arg not
/// provided falls back to its `default`; a required arg with neither is an error (exit-2 usage).
/// Unknown placeholders in the body are left as-is (honest, not silently dropped). Shared by prompt
/// and script. Returns the filled text.
fn fill_body(body: &str, args: &[PackageArg], provided: &[(String, String)]) -> Result<String, String> {
    let mut out = body.to_string();
    for arg in args {
        let value = provided
            .iter()
            .find(|(k, _)| k == &arg.name)
            .map(|(_, v)| v.clone())
            .or_else(|| arg.default.clone());
        match value {
            Some(v) => {
                out = out.replace(&format!("{{{{{}}}}}", arg.name), &v);
            }
            None if arg.required => {
                return Err(format!("missing required argument --{}", arg.name));
            }
            None => {
                out = out.replace(&format!("{{{{{}}}}}", arg.name), "");
            }
        }
    }
    Ok(out)
}

/// A prompt package as fetched from a registry (and persisted to the store as JSON).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct PromptPackage {
    /// The kebab-case package/command name.
    pub name: String,
    /// One-line description (the manifest synopsis).
    #[serde(default)]
    pub description: String,
    /// Optional model override (`--model` for the underlying `ask`); `None` ⇒ the session default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Declared arguments (empty ⇒ non-parameterized).
    #[serde(default)]
    pub arguments: Vec<PackageArg>,
    /// The prompt text, with `{{arg}}` placeholders for any declared arguments.
    pub body: String,
}

impl PromptPackage {
    /// Parse a package from registry JSON.
    pub fn from_json(bytes: &[u8]) -> Result<Self, String> {
        serde_json::from_slice(bytes).map_err(|e| format!("invalid package JSON: {e}"))
    }

    /// Serialize the package to pretty JSON (for the store).
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    /// The package's arguments rendered as manifest [`ParamSpec`]s.
    pub fn param_specs(&self) -> Vec<ParamSpec> {
        args_to_param_specs(&self.arguments)
    }

    /// Fill the body's `{{arg}}` placeholders (see [`fill_body`]).
    pub fn fill(&self, provided: &[(String, String)]) -> Result<String, String> {
        fill_body(&self.body, &self.arguments, provided)
    }
}

/// A shell-script package: its `body` is shell source run through the Brush engine on invocation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ScriptPackage {
    /// The kebab-case package/command name (the `/usr/bin/<name>` command).
    pub name: String,
    /// One-line description (the manifest synopsis).
    #[serde(default)]
    pub description: String,
    /// Declared arguments (empty ⇒ no templating).
    #[serde(default)]
    pub arguments: Vec<PackageArg>,
    /// The shell source, with `{{arg}}` placeholders for any declared arguments.
    pub body: String,
}

impl ScriptPackage {
    pub fn from_json(bytes: &[u8]) -> Result<Self, String> {
        serde_json::from_slice(bytes).map_err(|e| format!("invalid script package JSON: {e}"))
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    pub fn param_specs(&self) -> Vec<ParamSpec> {
        args_to_param_specs(&self.arguments)
    }

    /// Fill the shell source's `{{arg}}` placeholders (see [`fill_body`]).
    pub fn fill(&self, provided: &[(String, String)]) -> Result<String, String> {
        fill_body(&self.body, &self.arguments, provided)
    }
}

/// One reference document bundled in a skill (written verbatim under the skill dir).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct SkillDocument {
    /// Relative path under `/usr/share/skills/<name>/` (e.g. `SKILL.md`, `reference/api.md`).
    pub path: String,
    /// The document's verbatim content.
    pub content: String,
}

/// One shell script bundled in a skill (deposited to `/usr/share/skills/<name>/bin/<name>`, on `$PATH`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct SkillScript {
    /// The script's command name (the `bin/<name>` file).
    pub name: String,
    /// The shell source (written verbatim; runs as an ordinary `$PATH` command).
    pub body: String,
}

/// A skill package: a capability-context envelope (README:452). Not a command — surfaced to the model
/// as context, plus any bundled scripts land on `$PATH`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct SkillPackage {
    /// The kebab-case skill name (the `/usr/share/skills/<name>/` dir).
    pub name: String,
    /// One-line description (surfaced to the model).
    #[serde(default)]
    pub description: String,
    /// When the model should reach for this skill (surfaced to the model).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intended_use: Option<String>,
    /// Reference documents, written verbatim under the skill dir.
    #[serde(default)]
    pub documents: Vec<SkillDocument>,
    /// Bundled shell scripts, deposited to the skill's `bin/` (on `$PATH`).
    #[serde(default)]
    pub scripts: Vec<SkillScript>,
}

impl SkillPackage {
    pub fn from_json(bytes: &[u8]) -> Result<Self, String> {
        serde_json::from_slice(bytes).map_err(|e| format!("invalid skill package JSON: {e}"))
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }
}

/// Which of an MCP server's three artifact types this install exposes (`--tools`/`--prompts`/
/// `--resources`, default all). Selected at install and persisted so `load` reconstructs the same
/// surface. See README:657.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct McpArtifacts {
    #[serde(default = "yes")]
    pub tools: bool,
    #[serde(default = "yes")]
    pub prompts: bool,
    #[serde(default = "yes")]
    pub resources: bool,
}

fn yes() -> bool {
    true
}

impl Default for McpArtifacts {
    /// No selectors ⇒ all three (README: bare `grease install <server>` installs everything).
    fn default() -> Self {
        Self { tools: true, prompts: true, resources: true }
    }
}

impl McpArtifacts {
    /// Build from the parsed `--tools/--prompts/--resources` flags: if none were given, all three.
    pub fn from_flags(tools: bool, prompts: bool, resources: bool) -> Self {
        if !tools && !prompts && !resources {
            Self::default()
        } else {
            Self { tools, prompts, resources }
        }
    }
}

/// One tool cached in an installed MCP package's payload (a serde mirror of
/// [`crate::mcpclient::ToolSpec`] so `load()` rebuilds the tool surface without a live `tools/list`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct McpToolCache {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// The tool's JSON-schema input (lossless), as a JSON string (kept as a string so the payload is
    /// plain and diff-friendly; parsed back to a `Value` on load).
    #[serde(default)]
    pub input_schema: String,
}

/// One prompt cached in an installed MCP package's payload (name + description; the prompt body is
/// materialized as a standalone `PromptPackage` at install time, so only the listing is cached).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct McpPromptCache {
    pub name: String,
    #[serde(default)]
    pub description: String,
}

/// One resource cached in an installed MCP package's payload — the info needed to serve the
/// `/mnt/mcp/<server>/` virtual filesystem (listing + dynamic reads) without a live `resources/list`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct McpResourceCache {
    /// The MCP resource URI (`resources/read` target).
    pub uri: String,
    /// The relative path under `/mnt/mcp/<server>/` this resource is mounted at.
    pub rel_path: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// `true` if the resource was materialized as a real static file at install; `false` if it is
    /// dynamic (read live on each access via a top-level `cat` interception).
    #[serde(default)]
    pub is_static: bool,
}

/// An installed MCP-server package (grease type 2). The durable payload carries the server URL +
/// auth-env + which artifacts are exposed + the cached tool/prompt listings, so `GreaseState::load()`
/// rebuilds the surface on boot without a live re-fetch (MCP's own state is replay-only, not FS-backed).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct McpPackage {
    /// The kebab-case server/command name (the `/usr/lib/mcp/bin/<name>` command).
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// The HTTPS MCP endpoint (README: HTTPS-only, no stdio).
    pub url: String,
    /// The name of the env var holding the bearer token (never the token itself), if the server needs
    /// auth. Mirrors `McpServerConfig::auth_env`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_env: Option<String>,
    /// Which artifact types this install exposes.
    #[serde(default)]
    pub artifacts: McpArtifacts,
    /// Cached tool listing (populated at install from `tools/list`), so `type`/`man`/authz work offline.
    #[serde(default)]
    pub tools: Vec<McpToolCache>,
    /// Cached prompt listing (populated at install from `prompts/list`).
    #[serde(default)]
    pub prompts: Vec<McpPromptCache>,
    /// Cached resource listing (populated at install from `resources/list`), driving the
    /// `/mnt/mcp/<server>/` virtual-fs listing + dynamic reads.
    #[serde(default)]
    pub resources: Vec<McpResourceCache>,
}

impl McpPackage {
    pub fn from_json(bytes: &[u8]) -> Result<Self, String> {
        serde_json::from_slice(bytes).map_err(|e| format!("invalid mcp package JSON: {e}"))
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }
}

/// One method of a Golem agent, from its reflected metadata (name + declared parameter names). Drives
/// `--help`, arg validation, and the unknown-method error.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct AgentMethod {
    /// The kebab-case method name (the invocation subcommand).
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Declared parameter names (the `--<name> value` args this method accepts).
    #[serde(default)]
    pub params: Vec<String>,
}

/// A Golem-agent package (grease type 3). Registers the reflected metadata + creates a
/// `/usr/lib/agents/bin/<name>` command that invokes the agent type via wRPC (README:795).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct AgentPackage {
    /// The kebab-case command name (the `/usr/lib/agents/bin/<name>` command).
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// The Golem agent **type** name to invoke (the reflected type; may differ from the command name).
    pub agent_type: String,
    /// The declared constructor parameter names (the `--<name> value` flags that identify an instance).
    #[serde(default)]
    pub constructor_params: Vec<String>,
    /// The agent's methods (from reflected metadata).
    #[serde(default)]
    pub methods: Vec<AgentMethod>,
}

impl AgentPackage {
    pub fn from_json(bytes: &[u8]) -> Result<Self, String> {
        serde_json::from_slice(bytes).map_err(|e| format!("invalid agent package JSON: {e}"))
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    /// Look up a method by name.
    pub fn method(&self, name: &str) -> Option<&AgentMethod> {
        self.methods.iter().find(|m| m.name == name)
    }
}

/// The lowercase hex sha256 of `bytes` — for content integrity verification.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut s = String::with_capacity(64);
    for b in digest {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Verify a detached ed25519 signature over `body`. `sig_b64` is the base64 (standard alphabet)
/// detached signature the registry index advertises; `key_b64` is the base64 32-byte ed25519 public
/// key the registry was configured with (`grease registry add --key`). Uses `verify_strict` (rejects
/// weak/malleable keys). Returns `Ok(())` on a valid signature, `Err(reason)` otherwise — verify-only,
/// so no RNG and clean on wasm32-wasip2. See [[clank-grease]].
pub fn verify_signature(body: &[u8], sig_b64: &str, key_b64: &str) -> Result<(), String> {
    use base64::Engine;
    use ed25519_dalek::{Signature, VerifyingKey};

    let key_bytes = base64::engine::general_purpose::STANDARD
        .decode(key_b64.trim())
        .map_err(|e| format!("invalid public key (base64): {e}"))?;
    let key_arr: [u8; 32] = key_bytes
        .as_slice()
        .try_into()
        .map_err(|_| format!("public key must be 32 bytes, got {}", key_bytes.len()))?;
    let key =
        VerifyingKey::from_bytes(&key_arr).map_err(|e| format!("invalid ed25519 public key: {e}"))?;

    let sig_bytes = base64::engine::general_purpose::STANDARD
        .decode(sig_b64.trim())
        .map_err(|e| format!("invalid signature (base64): {e}"))?;
    let sig_arr: [u8; 64] = sig_bytes
        .as_slice()
        .try_into()
        .map_err(|_| format!("signature must be 64 bytes, got {}", sig_bytes.len()))?;
    let sig = Signature::from_bytes(&sig_arr);

    key.verify_strict(body, &sig).map_err(|_| "signature does not verify".to_string())
}

/// Validate that `key_b64` decodes to a well-formed 32-byte ed25519 public key (so `grease registry
/// add --key` rejects a typo at add time). Returns `Ok(())` if usable.
pub fn validate_public_key(key_b64: &str) -> Result<(), String> {
    use base64::Engine;
    use ed25519_dalek::VerifyingKey;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(key_b64.trim())
        .map_err(|e| format!("invalid public key (base64): {e}"))?;
    let arr: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| format!("public key must be 32 bytes, got {}", bytes.len()))?;
    VerifyingKey::from_bytes(&arr).map_err(|e| format!("invalid ed25519 public key: {e}"))?;
    Ok(())
}

/// Read the `kind` field of a raw registry payload without committing to a full parse (so
/// `grease install` can route to the right typed parser). Defaults to `Prompt` when the payload
/// carries no `kind` (a grease-v1 prompt payload).
pub fn payload_kind(bytes: &[u8]) -> Result<PackageKind, String> {
    let v: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| format!("invalid package JSON: {e}"))?;
    match v.get("kind").and_then(|k| k.as_str()) {
        None | Some("prompt") => Ok(PackageKind::Prompt),
        Some("script") => Ok(PackageKind::Script),
        Some("skill") => Ok(PackageKind::Skill),
        Some("mcp") => Ok(PackageKind::Mcp),
        Some("agent") => Ok(PackageKind::Agent),
        Some(other) => Err(format!("unknown package kind '{other}'")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkg(json: &str) -> PromptPackage {
        PromptPackage::from_json(json.as_bytes()).unwrap()
    }

    #[test]
    fn non_parameterized_body_is_verbatim() {
        let p = pkg(r#"{"name":"tldr","description":"summarize","body":"Summarize the transcript."}"#);
        assert!(p.arguments.is_empty());
        assert_eq!(p.fill(&[]).unwrap(), "Summarize the transcript.");
        assert_eq!(p.model, None);
    }

    #[test]
    fn parameterized_fill_substitutes_and_defaults() {
        let p = pkg(
            r#"{"name":"summarize","description":"d","model":"anthropic/claude-sonnet-5",
                "arguments":[
                  {"name":"file","required":true},
                  {"name":"length","required":false,"default":"medium"}],
                "body":"Summarize {{file}} at {{length}} length."}"#,
        );
        assert_eq!(p.model.as_deref(), Some("anthropic/claude-sonnet-5"));
        // Both provided.
        let filled = p.fill(&[("file".into(), "a.md".into()), ("length".into(), "short".into())]).unwrap();
        assert_eq!(filled, "Summarize a.md at short length.");
        // length falls back to its default.
        let filled = p.fill(&[("file".into(), "b.md".into())]).unwrap();
        assert_eq!(filled, "Summarize b.md at medium length.");
        // Missing required `file` errors.
        let err = p.fill(&[]).unwrap_err();
        assert!(err.contains("missing required argument --file"));
    }

    #[test]
    fn param_specs_map_arguments() {
        let p = pkg(
            r#"{"name":"x","body":"{{a}}","arguments":[
                {"name":"a","required":true},{"name":"b","required":false,"default":"z"}]}"#,
        );
        let specs = p.param_specs();
        assert_eq!(specs.len(), 2);
        let a = specs.iter().find(|s| s.name == "a").unwrap();
        assert!(a.required && matches!(a.ty, ParamType::String));
        let b = specs.iter().find(|s| s.name == "b").unwrap();
        assert!(!b.required && b.default.as_deref() == Some("z"));
    }

    #[test]
    fn json_round_trips() {
        let p = pkg(r#"{"name":"x","description":"d","body":"hi"}"#);
        let back = PromptPackage::from_json(p.to_json().as_bytes()).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn sha256_is_stable_and_correct() {
        // Known vector: sha256("abc").
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(sha256_hex(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    }

    // RFC 8032 ed25519 test vector 2: message is the single byte 0x72.
    const RFC8032_PUB: &str = "PUAXw+hDiVqStwqnTRt+vJyYLM8uxJaMwM1V8Sr0Zgw=";
    const RFC8032_SIG: &str =
        "kqAJqfDUyrhyDoILX2QlQKKye1QWUD+Ps3YiI+vbadoIWsHkPhWZbkWPNhPQ8R2MOHsurrQwKu6wDSkWErsMAA==";
    const RFC8032_MSG: &[u8] = &[0x72];

    #[test]
    fn verify_signature_accepts_a_valid_ed25519_signature() {
        assert!(verify_signature(RFC8032_MSG, RFC8032_SIG, RFC8032_PUB).is_ok());
    }

    #[test]
    fn verify_signature_rejects_tamper_and_bad_inputs() {
        // Wrong message → no verify.
        assert!(verify_signature(b"different", RFC8032_SIG, RFC8032_PUB).is_err());
        // Wrong key (all-zero, a valid point but not the signer) → no verify.
        use base64::Engine;
        let zero_key = base64::engine::general_purpose::STANDARD.encode([0u8; 32]);
        assert!(verify_signature(RFC8032_MSG, RFC8032_SIG, &zero_key).is_err());
        // Malformed base64 / wrong lengths are clean errors, not panics.
        assert!(verify_signature(RFC8032_MSG, "not-base64!!", RFC8032_PUB).is_err());
        assert!(verify_signature(RFC8032_MSG, RFC8032_SIG, "AAAA").is_err()); // key too short
    }

    #[test]
    fn payload_kind_defaults_to_prompt_and_reads_declared() {
        assert_eq!(payload_kind(br#"{"name":"x","body":"hi"}"#).unwrap(), PackageKind::Prompt);
        assert_eq!(
            payload_kind(br#"{"kind":"prompt","name":"x","body":"hi"}"#).unwrap(),
            PackageKind::Prompt
        );
        assert_eq!(
            payload_kind(br#"{"kind":"script","name":"x","body":"echo hi"}"#).unwrap(),
            PackageKind::Script
        );
        assert_eq!(
            payload_kind(br#"{"kind":"skill","name":"x"}"#).unwrap(),
            PackageKind::Skill
        );
        assert!(payload_kind(br#"{"kind":"wrpc","name":"x"}"#).is_err());
    }

    #[test]
    fn script_package_fills_and_specs() {
        let s = ScriptPackage::from_json(
            br#"{"kind":"script","name":"greet","description":"greet",
                 "arguments":[{"name":"who","required":true}],
                 "body":"echo hello {{who}}"}"#,
        )
        .unwrap();
        assert_eq!(s.fill(&[("who".into(), "world".into())]).unwrap(), "echo hello world");
        assert!(s.fill(&[]).unwrap_err().contains("missing required argument --who"));
        assert_eq!(s.param_specs().len(), 1);
        let back = ScriptPackage::from_json(s.to_json().as_bytes()).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn skill_package_round_trips_envelope_and_bundles() {
        let sk = SkillPackage::from_json(
            br##"{"kind":"skill","name":"code-review","description":"review code",
                 "intended-use":"when the user asks for a review",
                 "documents":[{"path":"SKILL.md","content":"# how to review"}],
                 "scripts":[{"name":"lint-all","body":"echo linting"}]}"##,
        )
        .unwrap();
        assert_eq!(sk.intended_use.as_deref(), Some("when the user asks for a review"));
        assert_eq!(sk.documents.len(), 1);
        assert_eq!(sk.documents[0].path, "SKILL.md");
        assert_eq!(sk.scripts.len(), 1);
        assert_eq!(sk.scripts[0].name, "lint-all");
        let back = SkillPackage::from_json(sk.to_json().as_bytes()).unwrap();
        assert_eq!(sk, back);
    }
}

