//! `grease` prompt-package types: the registry payload, `{{var}}` templating, and sha256 integrity.
//!
//! A prompt package is fetched from a registry as JSON (`{name, description, model?, arguments?,
//! body}`). Non-parameterized prompts have an empty `arguments` and the `body` is used verbatim;
//! parameterized prompts declare `arguments` and reference them as `{{name}}` in the `body`. The
//! `body` (after `{{var}}` substitution) is what a prompt invocation passes to `ask`.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::manifest::{ParamSpec, ParamType};

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
    pub arguments: Vec<PromptArg>,
    /// The prompt text, with `{{arg}}` placeholders for any declared arguments.
    pub body: String,
}

/// One declared prompt argument.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct PromptArg {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
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

    /// The package's arguments rendered as manifest [`ParamSpec`]s (drives `--help`, completion). All
    /// are `String`-typed (prompt args are free text); `required`/`default` carry through.
    pub fn param_specs(&self) -> Vec<ParamSpec> {
        self.arguments
            .iter()
            .map(|a| ParamSpec {
                name: a.name.clone(),
                ty: ParamType::String,
                required: a.required,
                default: a.default.clone(),
            })
            .collect()
    }

    /// Fill the body's `{{arg}}` placeholders from `provided` (arg name → value). A declared arg not
    /// provided falls back to its `default`; a required arg with neither is an error (exit-2 usage).
    /// Unknown placeholders in the body are left as-is (the model sees them — honest, not silently
    /// dropped). Returns the filled prompt text.
    pub fn fill(&self, provided: &[(String, String)]) -> Result<String, String> {
        let mut out = self.body.clone();
        for arg in &self.arguments {
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
                    // Optional with no default: substitute empty.
                    out = out.replace(&format!("{{{{{}}}}}", arg.name), "");
                }
            }
        }
        Ok(out)
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
}
