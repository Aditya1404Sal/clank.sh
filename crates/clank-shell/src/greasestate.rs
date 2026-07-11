//! `Session`-held view of installed grease packages, reconstructed from the **durable agent FS** on
//! boot (unlike [`crate::mcpstate`], which is replay-rebuilt in memory).
//!
//! The store is the source of truth: each installed prompt is `<store>/<name>/prompt.json` (the full
//! [`PromptPackage`]) plus `<etc>/<name>.toml` (an install marker). `load()` scans the etc dir and
//! reads each payload from the store; nothing is fetched here (install already persisted it), so the
//! reconstruction is deterministic and needs no HTTP replay. Dynamic manifests (for `type`/`man`/authz/
//! the `ask` tool surface) come from [`Self::all_manifests`], installed into the per-line [`crate::dynreg`]
//! slot the same way MCP servers are.

use serde::{Deserialize, Serialize};

use crate::greasepkg::PromptPackage;
use crate::manifest::{AuthorizationPolicy, ExecutionScope, Manifest};

/// The on-disk install marker (`<etc>/<name>.toml`) — the registry a package came from, for `info`/
/// `update`. The payload itself lives in the store (`prompt.json`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct InstallMarker {
    /// The registry URL the package was installed from (for `grease update`).
    pub registry: String,
    /// The sha256 of the payload at install time (integrity + change detection).
    pub sha256: String,
}

/// One installed prompt: its marker + the full package payload.
#[derive(Clone, Debug)]
pub struct InstalledPrompt {
    pub marker: InstallMarker,
    pub package: PromptPackage,
}

/// The installed-package view, held on the `Session`.
#[derive(Default)]
pub struct GreaseState {
    prompts: Vec<InstalledPrompt>,
}

impl GreaseState {
    /// Reconstruct the installed set by scanning the etc dir for `<name>.toml` markers and reading
    /// each package's payload from the store. Corrupt/partial installs are skipped (not fatal).
    pub fn load() -> Self {
        let mut prompts = Vec::new();
        let etc = crate::greaseconfig::etc_dir();
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
                    prompts.push(p);
                }
            }
        }
        prompts.sort_by(|a, b| a.package.name.cmp(&b.package.name));
        Self { prompts }
    }

    /// Insert or replace an installed prompt (after a successful install).
    pub fn set_installed(&mut self, prompt: InstalledPrompt) {
        self.remove(&prompt.package.name);
        self.prompts.push(prompt);
        self.prompts.sort_by(|a, b| a.package.name.cmp(&b.package.name));
    }

    /// Drop an installed prompt from the in-memory view (the caller removes the files).
    pub fn remove(&mut self, name: &str) {
        self.prompts.retain(|p| p.package.name != name);
    }

    pub fn prompts(&self) -> &[InstalledPrompt] {
        &self.prompts
    }

    pub fn get(&self, name: &str) -> Option<&InstalledPrompt> {
        self.prompts.iter().find(|p| p.package.name == name)
    }

    /// Whether `name` is an installed prompt (drives command dispatch).
    pub fn is_prompt(&self, name: &str) -> bool {
        self.prompts.iter().any(|p| p.package.name == name)
    }

    /// The dynamic manifest for an installed prompt: `Subprocess` scope, `Confirm` policy (running a
    /// prompt is an outbound LLM call, like `ask`), the declared arguments as the input schema.
    pub fn manifest_for(&self, name: &str) -> Option<Manifest> {
        let p = self.get(name)?;
        let synopsis = if p.package.description.is_empty() {
            format!("installed prompt ({name})")
        } else {
            p.package.description.clone()
        };
        Some(
            Manifest::builtin(name, synopsis)
                .with_scope(ExecutionScope::Subprocess)
                .with_policy(AuthorizationPolicy::Confirm)
                .with_params(p.package.param_specs())
                .with_help(self.prompt_help(name).unwrap_or_default()),
        )
    }

    /// The dynamic manifests for all installed prompts (for the per-line [`crate::dynreg`] slot).
    pub fn all_manifests(&self) -> Vec<Manifest> {
        self.prompts.iter().filter_map(|p| self.manifest_for(&p.package.name)).collect()
    }

    /// Every installed prompt as an [`crate::askcmd::AskTool`], so the model can invoke prompts as
    /// tools. The tool name is the prompt name; the parameters schema is a JSON-schema object built
    /// from the declared arguments.
    pub fn ask_tool_definitions(&self) -> Vec<crate::askcmd::AskTool> {
        self.prompts
            .iter()
            .map(|p| {
                let props: serde_json::Map<String, serde_json::Value> = p
                    .package
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
                    p.package.arguments.iter().filter(|a| a.required).map(|a| a.name.as_str()).collect();
                let schema = serde_json::json!({
                    "type": "object", "properties": props, "required": required
                });
                crate::askcmd::AskTool {
                    name: p.package.name.clone(),
                    description: format!("[prompt] {}", p.package.description),
                    parameters_schema: schema.to_string(),
                }
            })
            .collect()
    }

    /// Human-facing help for an installed prompt: its description + declared arguments.
    pub fn prompt_help(&self, name: &str) -> Option<String> {
        let p = self.get(name)?;
        let mut out = format!("{name} — {}\n", p.package.description);
        if p.package.arguments.is_empty() {
            out.push_str("\nNo arguments; runs the prompt against the AI model.\n");
        } else {
            out.push_str("\nArguments:\n");
            for a in &p.package.arguments {
                let req = if a.required { " (required)" } else { "" };
                let def = a.default.as_deref().map(|d| format!(" [default: {d}]")).unwrap_or_default();
                out.push_str(&format!("  --{} — {}{req}{def}\n", a.name, a.description));
            }
        }
        out.push_str(&format!(
            "\nRunning `{name}` sends the prompt to the AI model (outbound HTTP; confirms unless run \
             with sudo). Installed by grease from {}.\n",
            p.marker.registry
        ));
        Some(out)
    }
}

/// Load one installed prompt (`<etc>/<name>.toml` marker + `<store>/<name>/prompt.json` payload).
/// `None` if either file is missing or unparseable.
fn load_one(name: &str) -> Option<InstalledPrompt> {
    let marker_path = crate::greaseconfig::etc_dir().join(format!("{name}.toml"));
    let marker: InstallMarker = toml::from_str(&std::fs::read_to_string(marker_path).ok()?).ok()?;
    let payload_path = crate::greaseconfig::store_dir().join(name).join("prompt.json");
    let package = PromptPackage::from_json(&std::fs::read(payload_path).ok()?).ok()?;
    Some(InstalledPrompt { marker, package })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::greasepkg::PromptArg;

    fn sample() -> PromptPackage {
        PromptPackage {
            name: "summarize".into(),
            description: "summarize a file".into(),
            model: None,
            arguments: vec![PromptArg {
                name: "file".into(),
                description: "the file".into(),
                required: true,
                default: None,
            }],
            body: "Summarize {{file}}.".into(),
        }
    }

    #[test]
    fn manifest_and_tools_reflect_the_package() {
        let mut state = GreaseState::default();
        state.set_installed(InstalledPrompt {
            marker: InstallMarker { registry: "https://r".into(), sha256: "abc".into() },
            package: sample(),
        });
        assert!(state.is_prompt("summarize"));

        let m = state.manifest_for("summarize").unwrap();
        assert_eq!(m.authorization_policy, AuthorizationPolicy::Confirm);
        assert_eq!(m.execution_scope, ExecutionScope::Subprocess);
        assert_eq!(m.input_schema.len(), 1);
        assert_eq!(m.input_schema[0].name, "file");

        let tools = state.ask_tool_definitions();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "summarize");
        assert!(tools[0].parameters_schema.contains("file"));

        let help = state.prompt_help("summarize").unwrap();
        assert!(help.contains("--file") && help.contains("required"));

        state.remove("summarize");
        assert!(!state.is_prompt("summarize"));
        assert!(state.manifest_for("summarize").is_none());
    }
}
