//! `Session`-held view of installed grease packages, reconstructed from the **durable agent FS** on
//! boot (unlike [`crate::mcp::state`], which is replay-rebuilt in memory).
//!
//! The store is the source of truth: each installed package is `<store>/<name>/<kind>.json` (the full
//! typed payload) plus `<etc>/<name>.toml` (an install marker carrying the [`PackageKind`]). `load()`
//! scans the etc dir and reads each payload from the store per its marker's kind; nothing is fetched
//! here (install already persisted it), so the reconstruction is deterministic and needs no HTTP
//! replay. Dynamic manifests (for `type`/`man`/authz/the `ask` tool surface) come from
//! [`Self::all_manifests`], installed into the per-line [`crate::dynreg`] slot the same way MCP
//! servers are. Skills are the exception: they are **not commands** — they contribute a system-prompt
//! listing (via [`Self::skills`]) but no manifest and no `ask` tool.

use serde::{Deserialize, Serialize};

use crate::grease::pkg::{
    AgentPackage, McpPackage, PackageKind, PromptPackage, ScriptPackage, SkillPackage,
};
use crate::manifest::{AuthorizationPolicy, ExecutionScope, Manifest};

/// The on-disk install marker (`<etc>/<name>.toml`) — the package kind + the registry it came from +
/// its verified sha256.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct InstallMarker {
    /// Which kind of package this is (drives the payload parser + run path). Defaults to `Prompt`
    /// so kind-less markers written by grease v1 still load.
    #[serde(default)]
    pub kind: PackageKind,
    /// The registry URL the package was installed from (for `grease update`).
    pub registry: String,
    /// The sha256 of the payload at install time (integrity + change detection).
    pub sha256: String,
    /// Whether the sha256 was verified against the registry's advertised hash (`false` = record-only,
    /// the registry index carried no hash). Defaults false for markers written before this field.
    #[serde(default)]
    pub verified: bool,
    /// Whether the payload's ed25519 signature was verified against the registry's trusted key
    /// (`grease registry add --key`). `false` = the registry is unsigned (no key configured), or the
    /// marker predates signing. Defaults false.
    #[serde(default)]
    pub signature_verified: bool,
    /// The signer identity the registry index advertised for this package (`signer` field), recorded
    /// for `info`/`list` when the signature verified. `None` = unsigned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signer: Option<String>,
    /// Whether the payload's RFC-6962 transparency-log inclusion proof verified against the registry's
    /// advertised Merkle root. `false` = the registry runs no log, or the marker predates this field.
    #[serde(default)]
    pub log_verified: bool,
    /// The transparency-log leaf index the package was recorded at (when log-verified).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_index: Option<u64>,
}

/// The typed payload of an installed package, tagged by kind.
#[derive(Clone, Debug)]
pub enum Payload {
    Prompt(PromptPackage),
    Script(ScriptPackage),
    Skill(SkillPackage),
    Mcp(McpPackage),
    Agent(AgentPackage),
}

impl Payload {
    pub fn kind(&self) -> PackageKind {
        match self {
            Payload::Prompt(_) => PackageKind::Prompt,
            Payload::Script(_) => PackageKind::Script,
            Payload::Skill(_) => PackageKind::Skill,
            Payload::Mcp(_) => PackageKind::Mcp,
            Payload::Agent(_) => PackageKind::Agent,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Payload::Prompt(p) => &p.name,
            Payload::Script(s) => &s.name,
            Payload::Skill(s) => &s.name,
            Payload::Mcp(m) => &m.name,
            Payload::Agent(a) => &a.name,
        }
    }

    pub fn description(&self) -> &str {
        match self {
            Payload::Prompt(p) => &p.description,
            Payload::Script(s) => &s.description,
            Payload::Skill(s) => &s.description,
            Payload::Mcp(m) => &m.description,
            Payload::Agent(a) => &a.description,
        }
    }
}

/// One installed package: its marker + the typed payload.
#[derive(Clone, Debug)]
pub struct InstalledPackage {
    pub marker: InstallMarker,
    pub payload: Payload,
}

impl InstalledPackage {
    pub fn name(&self) -> &str {
        self.payload.name()
    }
    pub fn kind(&self) -> PackageKind {
        self.payload.kind()
    }
}

/// The installed-package view, held on the `Session`.
#[derive(Default)]
pub struct GreaseState {
    packages: Vec<InstalledPackage>,
    /// Monotonic counter bumped on every mutation, so the `Session` can cache capability views
    /// (manifests / system prompt / resource index) and rebuild only when the package set changes.
    version: u64,
}

impl GreaseState {
    /// Reconstruct the installed set by scanning the etc dir for `<name>.toml` markers and reading
    /// each package's payload from the store per its kind. Corrupt/partial installs are skipped.
    pub fn load() -> Self {
        let mut packages = Vec::new();
        let etc = crate::grease::config::etc_dir();
        if let Ok(entries) = std::fs::read_dir(&etc) {
            for entry in entries.flatten() {
                let path = entry.path();
                // Skip registries.toml and non-toml files.
                if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                    continue;
                }
                let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                if name == "registries" {
                    continue;
                }
                if let Some(p) = load_one(name) {
                    packages.push(p);
                }
            }
        }
        packages.sort_by(|a, b| a.name().cmp(b.name()));
        Self { packages, version: 0 }
    }

    /// The mutation counter — see [`version`](Self::version)'s field docs. Any change to the package
    /// set bumps it, so a cache keyed on it is invalidated exactly when a capability view would change.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Insert or replace an installed package (after a successful install).
    pub fn set_installed(&mut self, pkg: InstalledPackage) {
        let name = pkg.name().to_string();
        self.remove(&name);
        self.packages.push(pkg);
        self.packages.sort_by(|a, b| a.name().cmp(b.name()));
        self.version = self.version.wrapping_add(1);
    }

    /// Drop an installed package from the in-memory view (the caller removes the files).
    pub fn remove(&mut self, name: &str) {
        self.packages.retain(|p| p.name() != name);
        self.version = self.version.wrapping_add(1);
    }

    pub fn packages(&self) -> &[InstalledPackage] {
        &self.packages
    }

    pub fn get(&self, name: &str) -> Option<&InstalledPackage> {
        self.packages.iter().find(|p| p.name() == name)
    }

    /// The kind of an installed package, if any.
    pub fn kind_of(&self, name: &str) -> Option<PackageKind> {
        self.get(name).map(|p| p.kind())
    }

    /// Whether `name` is an installed prompt (drives command dispatch).
    pub fn is_prompt(&self, name: &str) -> bool {
        self.kind_of(name) == Some(PackageKind::Prompt)
    }

    /// Whether `name` is an installed script (drives command dispatch).
    pub fn is_script(&self, name: &str) -> bool {
        self.kind_of(name) == Some(PackageKind::Script)
    }

    /// Whether `name` is an installed skill (skills are not commands — for `info`/`remove`).
    pub fn is_skill(&self, name: &str) -> bool {
        self.kind_of(name) == Some(PackageKind::Skill)
    }

    /// Whether `name` is an installed MCP-server package (for `info`/`remove`/boot reconstruction).
    pub fn is_mcp(&self, name: &str) -> bool {
        self.kind_of(name) == Some(PackageKind::Mcp)
    }

    /// Whether `name` is an installed Golem-agent package (drives the `run_agent` dispatch).
    pub fn is_agent(&self, name: &str) -> bool {
        self.kind_of(name) == Some(PackageKind::Agent)
    }

    /// The installed Golem-agent payload, if `name` is an agent.
    pub fn agent(&self, name: &str) -> Option<&AgentPackage> {
        match self.get(name).map(|p| &p.payload) {
            Some(Payload::Agent(a)) => Some(a),
            _ => None,
        }
    }

    /// The installed prompt payload, if `name` is a prompt.
    pub fn prompt(&self, name: &str) -> Option<&PromptPackage> {
        match self.get(name).map(|p| &p.payload) {
            Some(Payload::Prompt(p)) => Some(p),
            _ => None,
        }
    }

    /// The installed script payload, if `name` is a script.
    pub fn script(&self, name: &str) -> Option<&ScriptPackage> {
        match self.get(name).map(|p| &p.payload) {
            Some(Payload::Script(s)) => Some(s),
            _ => None,
        }
    }

    /// The installed skill payload, if `name` is a skill.
    pub fn skill(&self, name: &str) -> Option<&SkillPackage> {
        match self.get(name).map(|p| &p.payload) {
            Some(Payload::Skill(s)) => Some(s),
            _ => None,
        }
    }

    /// All installed skills (skills are not commands; they contribute a system-prompt listing only).
    pub fn skills(&self) -> Vec<&SkillPackage> {
        self.packages
            .iter()
            .filter_map(|p| match &p.payload {
                Payload::Skill(s) => Some(s),
                _ => None,
            })
            .collect()
    }

    /// The installed MCP-server payload, if `name` is an MCP package.
    pub fn mcp(&self, name: &str) -> Option<&McpPackage> {
        match self.get(name).map(|p| &p.payload) {
            Some(Payload::Mcp(m)) => Some(m),
            _ => None,
        }
    }

    /// Find an installed MCP resource-template executable by its `<server>-<name>` command name,
    /// returning `(server_url, auth_env, uri_template)` for running it.
    pub fn mcp_template(&self, cmd: &str) -> Option<(String, Option<String>, String)> {
        for m in self.mcp_packages() {
            if let Some(t) = m.templates.iter().find(|t| t.name == cmd) {
                return Some((m.url.clone(), m.auth_env.clone(), t.uri_template.clone()));
            }
        }
        None
    }

    /// Whether `cmd` is an installed MCP resource-template executable.
    pub fn is_mcp_template(&self, cmd: &str) -> bool {
        self.mcp_packages().iter().any(|m| m.templates.iter().any(|t| t.name == cmd))
    }

    /// Look up a resource entry by its `/mnt/mcp/<server>/<rel_path>` — for `mcp resource info`/`stat`.
    pub fn mcp_resource_entry(
        &self,
        server: &str,
        rel_path: &str,
    ) -> Option<&crate::grease::pkg::McpResourceCache> {
        self.mcp(server)?.resources.iter().find(|r| r.rel_path == rel_path)
    }

    /// All installed MCP-server packages (for boot reconstruction into `McpState`).
    pub fn mcp_packages(&self) -> Vec<&McpPackage> {
        self.packages
            .iter()
            .filter_map(|p| match &p.payload {
                Payload::Mcp(m) => Some(m),
                _ => None,
            })
            .collect()
    }

    /// The flattened MCP resource index (all installed servers' resources) for the `/mnt/mcp`
    /// virtual-fs ([`crate::mcpfs`]).
    pub fn mcp_resource_index(&self) -> Vec<crate::mcpfs::ResourceEntry> {
        let mut out = Vec::new();
        for m in self.mcp_packages() {
            for r in &m.resources {
                let mut e =
                    crate::mcpfs::ResourceEntry::plain(&m.name, &r.rel_path, &r.uri, r.is_static);
                e.last_modified = r.last_modified.clone();
                e.audience = r.audience.clone();
                e.priority = r.priority;
                e.size = r.size;
                e.description = r.description.clone();
                out.push(e);
            }
            // Templates appear as stubs in `/mnt/mcp/<server>/` (README:774) — rel_path is the
            // `<server>-<name>` executable name; the URI carries the template for display.
            for t in &m.templates {
                let mut e = crate::mcpfs::ResourceEntry::plain(&m.name, &t.name, &t.uri_template, false);
                e.is_template = true;
                e.description = t.description.clone();
                out.push(e);
            }
        }
        out
    }

    /// The dynamic manifest for an installed command package (prompt or script). Both are
    /// `Subprocess`/`Confirm` (a prompt makes an outbound LLM call; a script runs local shell), with
    /// the declared arguments as the input schema. **Skills return `None`** — they are not commands.
    pub fn manifest_for(&self, name: &str) -> Option<Manifest> {
        let pkg = self.get(name)?;
        let (synopsis, params): (String, Vec<crate::manifest::ParamSpec>) = match &pkg.payload {
            Payload::Prompt(p) => (fallback_synopsis(name, &p.description, "prompt"), p.param_specs()),
            Payload::Script(s) => (fallback_synopsis(name, &s.description, "script"), s.param_specs()),
            Payload::Agent(a) => {
                // An agent IS a command (remote wRPC invocation → Confirm). Its input schema is the
                // constructor params (the instance-identifying flags); method args are per-method.
                let params = a
                    .constructor_params
                    .iter()
                    .map(|n| crate::manifest::ParamSpec {
                        name: n.clone(),
                        ty: crate::manifest::ParamType::String,
                        required: false,
                        default: None,
                    })
                    .collect();
                (fallback_synopsis(name, &a.description, "agent"), params)
            }
            // A skill isn't a command; an MCP server's command surface lives in `McpState` (the server
            // + its tool subcommands are registered there at install, with their own bin stub) — grease
            // is only the durable-persistence layer, so it contributes no dynreg manifest here.
            Payload::Skill(_) | Payload::Mcp(_) => return None,
        };
        Some(
            Manifest::builtin(name, synopsis)
                .with_scope(ExecutionScope::Subprocess)
                .with_policy(AuthorizationPolicy::Confirm)
                .with_params(params)
                .with_help(self.pkg_help(name).unwrap_or_default()),
        )
    }

    /// The dynamic manifests for all installed command packages (prompts + scripts) — for the
    /// per-line [`crate::dynreg`] slot. Skills contribute nothing here.
    pub fn all_manifests(&self) -> Vec<Manifest> {
        self.packages.iter().filter_map(|p| self.manifest_for(p.name())).collect()
    }

    /// Installed prompts (and only prompts) as [`crate::ai::ask::AskTool`]s, so the model can invoke
    /// prompts as tools. The tool name is namespaced `prompt__<name>` (mirroring `mcp__<server>__<tool>`;
    /// the executor decodes it back to a `<name> --arg value …` line). **Scripts and skills are
    /// excluded** — scripts run local shell and are reachable via the plain `shell` tool; skills are
    /// context, not tools.
    pub fn ask_tool_definitions(&self) -> Vec<crate::ai::ask::AskTool> {
        self.packages
            .iter()
            .filter_map(|pkg| match &pkg.payload {
                Payload::Prompt(p) => Some(p),
                _ => None,
            })
            .map(|p| {
                let props: serde_json::Map<String, serde_json::Value> = p
                    .arguments
                    .iter()
                    .map(|a| {
                        (
                            a.name.clone(),
                            serde_json::json!({ "type": "string", "description": a.description }),
                        )
                    })
                    .collect();
                let required: Vec<&str> =
                    p.arguments.iter().filter(|a| a.required).map(|a| a.name.as_str()).collect();
                let schema = serde_json::json!({
                    "type": "object", "properties": props, "required": required
                });
                crate::ai::ask::AskTool {
                    name: format!("prompt__{}", p.name),
                    description: format!("[prompt] {}", p.description),
                    parameters_schema: schema.to_string(),
                }
            })
            .collect()
    }

    /// Human-facing help for an installed command package (prompt or script). `None` for a skill (not
    /// a command) or an unknown name.
    pub fn pkg_help(&self, name: &str) -> Option<String> {
        let pkg = self.get(name)?;
        match &pkg.payload {
            Payload::Prompt(p) => Some(self.prompt_help_text(name, p, &pkg.marker)),
            Payload::Script(s) => Some(self.script_help_text(name, s, &pkg.marker)),
            Payload::Agent(a) => Some(self.agent_help_text(name, a, &pkg.marker)),
            // A skill isn't a command; an MCP server's help comes from the `mcp` server bin stub —
            // `grease info` handles these via their own describers.
            Payload::Skill(_) | Payload::Mcp(_) => None,
        }
    }

    fn agent_help_text(&self, name: &str, a: &AgentPackage, marker: &InstallMarker) -> String {
        let mut out = format!("{name} — {} [agent]\n", a.description);
        out.push_str(&format!("\nAgent type: {}\n", a.agent_type));
        if !a.constructor_params.is_empty() {
            out.push_str(&format!(
                "\nConstructor flags (identify the instance): {}\n",
                a.constructor_params.iter().map(|p| format!("--{p}")).collect::<Vec<_>>().join(" ")
            ));
        }
        if a.methods.is_empty() {
            out.push_str("\nNo methods declared.\n");
        } else {
            out.push_str("\nMethods:\n");
            for m in &a.methods {
                let params = if m.params.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", m.params.iter().map(|p| format!("--{p}")).collect::<Vec<_>>().join(" "))
                };
                out.push_str(&format!("  {} — {}{params}\n", m.name, m.description));
            }
        }
        out.push_str(&format!(
            "\nUsage: {name} [--<ctor> val …] <method> [-- --<arg> val …]\n\
             Running `{name}` invokes the agent in the Golem cluster (confirms unless run with sudo; \
             requires a cluster). Installed by grease from {} [{}].\n",
            marker.registry,
            integrity_note(marker),
        ));
        out
    }

    fn prompt_help_text(&self, name: &str, p: &PromptPackage, marker: &InstallMarker) -> String {
        let mut out = format!("{name} — {}\n", p.description);
        if p.arguments.is_empty() {
            out.push_str("\nNo arguments; runs the prompt against the AI model.\n");
        } else {
            out.push_str("\nArguments:\n");
            for a in &p.arguments {
                let req = if a.required { " (required)" } else { "" };
                let def = a.default.as_deref().map(|d| format!(" [default: {d}]")).unwrap_or_default();
                out.push_str(&format!("  --{} — {}{req}{def}\n", a.name, a.description));
            }
        }
        out.push_str(&format!(
            "\nRunning `{name}` sends the prompt to the AI model (outbound HTTP; confirms unless run \
             with sudo). Installed by grease from {} [{}].\n",
            marker.registry,
            integrity_note(marker),
        ));
        out
    }

    fn script_help_text(&self, name: &str, s: &ScriptPackage, marker: &InstallMarker) -> String {
        let mut out = format!("{name} — {}\n", s.description);
        if s.arguments.is_empty() {
            out.push_str("\nNo arguments.\n");
        } else {
            out.push_str("\nArguments:\n");
            for a in &s.arguments {
                let req = if a.required { " (required)" } else { "" };
                let def = a.default.as_deref().map(|d| format!(" [default: {d}]")).unwrap_or_default();
                out.push_str(&format!("  --{} — {}{req}{def}\n", a.name, a.description));
            }
        }
        out.push_str(&format!(
            "\nRunning `{name}` executes local shell commands (confirms unless run with sudo). \
             Installed by grease from {} [{}].\n",
            marker.registry,
            integrity_note(marker),
        ));
        out
    }
}

/// The manifest synopsis for a package, falling back to a kind-labelled placeholder if it has no
/// description.
fn fallback_synopsis(name: &str, description: &str, kind: &str) -> String {
    if description.is_empty() {
        format!("installed {kind} ({name})")
    } else {
        description.to_string()
    }
}

/// The integrity note used in help text: the sha256 status, the signer (when signed), and the
/// transparency-log leaf index (when log-audited).
fn integrity_note(marker: &InstallMarker) -> String {
    let status = if marker.verified { "verified" } else { "unverified" };
    let sha = format!("sha256 {} ({status})", &marker.sha256[..marker.sha256.len().min(12)]);
    let mut out = if marker.signature_verified {
        let signer = marker.signer.as_deref().unwrap_or("registry key");
        format!("{sha}, signed by {signer}")
    } else {
        format!("{sha}, unsigned")
    };
    if marker.log_verified {
        match marker.log_index {
            Some(idx) => out.push_str(&format!(", in transparency log @{idx}")),
            None => out.push_str(", in transparency log"),
        }
    }
    out
}

/// Load one installed package (`<etc>/<name>.toml` marker + `<store>/<name>/<kind>.json` payload).
/// `None` if either file is missing or unparseable. Dispatches on the marker's `kind`.
fn load_one(name: &str) -> Option<InstalledPackage> {
    let marker_path = crate::grease::config::etc_dir().join(format!("{name}.toml"));
    let marker: InstallMarker = toml::from_str(&std::fs::read_to_string(marker_path).ok()?).ok()?;
    let payload_path =
        crate::grease::config::store_dir().join(name).join(marker.kind.payload_file());
    let bytes = std::fs::read(payload_path).ok()?;
    let payload = match marker.kind {
        PackageKind::Prompt => Payload::Prompt(PromptPackage::from_json(&bytes).ok()?),
        PackageKind::Script => Payload::Script(ScriptPackage::from_json(&bytes).ok()?),
        PackageKind::Skill => Payload::Skill(SkillPackage::from_json(&bytes).ok()?),
        PackageKind::Mcp => Payload::Mcp(McpPackage::from_json(&bytes).ok()?),
        PackageKind::Agent => Payload::Agent(AgentPackage::from_json(&bytes).ok()?),
    };
    Some(InstalledPackage { marker, payload })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grease::pkg::PackageArg;

    fn sample_prompt() -> PromptPackage {
        PromptPackage {
            name: "summarize".into(),
            description: "summarize a file".into(),
            model: None,
            arguments: vec![PackageArg {
                name: "file".into(),
                description: "the file".into(),
                required: true,
                default: None,
            }],
            body: "Summarize {{file}}.".into(),
        }
    }

    fn marker(kind: PackageKind) -> InstallMarker {
        InstallMarker {
            kind,
            registry: "https://r".into(),
            sha256: "abcdef012345".into(),
            verified: true,
            signature_verified: false,
            signer: None,
            log_verified: false,
            log_index: None,
        }
    }

    #[test]
    fn manifest_and_tools_reflect_the_prompt() {
        let mut state = GreaseState::default();
        state.set_installed(InstalledPackage {
            marker: marker(PackageKind::Prompt),
            payload: Payload::Prompt(sample_prompt()),
        });
        assert!(state.is_prompt("summarize"));
        assert!(!state.is_script("summarize"));

        let m = state.manifest_for("summarize").unwrap();
        assert_eq!(m.authorization_policy, AuthorizationPolicy::Confirm);
        assert_eq!(m.execution_scope, ExecutionScope::Subprocess);
        assert_eq!(m.input_schema.len(), 1);
        assert_eq!(m.input_schema[0].name, "file");

        let tools = state.ask_tool_definitions();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "prompt__summarize");
        assert!(tools[0].parameters_schema.contains("file"));

        let help = state.pkg_help("summarize").unwrap();
        assert!(help.contains("--file") && help.contains("required"));

        state.remove("summarize");
        assert!(!state.is_prompt("summarize"));
        assert!(state.manifest_for("summarize").is_none());
    }

    #[test]
    fn version_bumps_on_install_and_remove() {
        // The Session's capability cache keys on version(); install + remove must each bump it so the
        // system prompt / manifests / resource index rebuild when the package set changes.
        let mut state = GreaseState::default();
        let v0 = state.version();
        state.set_installed(InstalledPackage {
            marker: marker(PackageKind::Prompt),
            payload: Payload::Prompt(sample_prompt()),
        });
        let v1 = state.version();
        assert_ne!(v1, v0, "set_installed must bump the version");
        state.remove("summarize");
        assert_ne!(state.version(), v1, "remove must bump the version");
    }

    #[test]
    fn script_is_a_confirm_command_but_not_an_ask_tool() {
        let mut state = GreaseState::default();
        state.set_installed(InstalledPackage {
            marker: marker(PackageKind::Script),
            payload: Payload::Script(ScriptPackage {
                name: "greet".into(),
                description: "greet".into(),
                arguments: vec![PackageArg {
                    name: "who".into(),
                    description: "who".into(),
                    required: true,
                    default: None,
                }],
                body: "echo hello {{who}}".into(),
            }),
        });
        assert!(state.is_script("greet"));
        assert!(!state.is_prompt("greet"));

        let m = state.manifest_for("greet").unwrap();
        assert_eq!(m.authorization_policy, AuthorizationPolicy::Confirm);
        assert_eq!(m.execution_scope, ExecutionScope::Subprocess);

        // A script is NOT surfaced as a `prompt__` ask tool.
        assert!(state.ask_tool_definitions().is_empty());
        // Its manifest IS registered dynamically (so `type`/authz see it).
        assert_eq!(state.all_manifests().len(), 1);

        let help = state.pkg_help("greet").unwrap();
        assert!(help.contains("executes local shell commands"));
    }

    #[test]
    fn skill_is_not_a_command_and_lists_in_skills() {
        let mut state = GreaseState::default();
        state.set_installed(InstalledPackage {
            marker: marker(PackageKind::Skill),
            payload: Payload::Skill(SkillPackage {
                name: "code-review".into(),
                description: "review code".into(),
                intended_use: Some("when reviewing".into()),
                documents: vec![],
                scripts: vec![],
            }),
        });
        assert!(state.is_skill("code-review"));
        // A skill is NOT a command: no manifest, no ask tool.
        assert!(state.manifest_for("code-review").is_none());
        assert!(state.all_manifests().is_empty());
        assert!(state.ask_tool_definitions().is_empty());
        assert!(state.pkg_help("code-review").is_none());
        // But it IS listed as a skill for the system prompt.
        let skills = state.skills();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "code-review");
        assert_eq!(skills[0].intended_use.as_deref(), Some("when reviewing"));
    }
}
