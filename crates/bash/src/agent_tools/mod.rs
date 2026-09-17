//! Transport-independent CLI projection of an owner's bound tools.
//!
//! The transport derives fields from its authoritative canonical input model. The shell only
//! handles command surfaces and ordinary JSON values; no Golem host types cross this boundary.

#![allow(missing_docs)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

mod builtin;
mod parser;
#[cfg(test)]
mod tests;

pub(crate) use builtin::install;
pub(crate) use builtin::registration;
pub(crate) use builtin::suspend;
pub(crate) use builtin::words;
pub use parser::{parse, Projection};

/// Maximum retained bytes in one tool attachment.
pub const MAX_ATTACHMENT_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Argument {
    pub name: String,
    pub aliases: Vec<String>,
    pub short: Option<char>,
    pub kind: String,
    pub schema: Value,
    pub spec: Value,
    pub default: Option<Value>,
    pub env_var: Option<String>,
    pub required: bool,
    pub doc: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCommand {
    pub path: Vec<String>,
    pub fields: Vec<Argument>,
    /// Closed canonical graph, retained as plain data for schema references and coercion.
    pub schema: Value,
    pub constraints: Vec<Value>,
    pub read_only: bool,
    pub stdin: bool,
    pub stdout: bool,
    pub errors: BTreeMap<String, u8>,
    pub metadata: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolNode {
    pub name: String,
    pub aliases: Vec<String>,
    pub doc: String,
    pub children: Vec<usize>,
    pub globals: Vec<Argument>,
    pub command: Option<ToolCommand>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolDefinition {
    /// Host lookup identity; it may differ from the descriptor's root name.
    pub name: String,
    pub version: String,
    pub nodes: Vec<ToolNode>,
}

impl ToolDefinition {
    pub fn help(&self, index: usize) -> String {
        use std::fmt::Write;
        let node = &self.nodes[index];
        let mut out = format!("{} {}\n{}\n", self.name, self.version, node.doc);
        let mut path = Vec::new();
        self.find_path(0, index, &mut path);
        let _ = write!(out, "\nUsage: {}", self.name);
        for part in &path {
            let _ = write!(out, " {part}");
        }
        if let Some(command) = &node.command {
            for field in &command.fields {
                match field.kind.as_str() {
                    "positional" => {
                        let _ = write!(out, " <{}>", field.name);
                    }
                    "tail" => {
                        let _ = write!(out, " [{}...]", field.name);
                    }
                    _ => {
                        let _ = write!(out, " [--{}]", field.name);
                    }
                }
            }
            out.push_str("\n\nArguments and options:\n");
            for field in &command.fields {
                let _ = writeln!(
                    out,
                    "  {}{} ({}){}{} — {}",
                    if matches!(field.kind.as_str(), "positional" | "tail") {
                        ""
                    } else {
                        "--"
                    },
                    field.name,
                    field.schema["kind"].as_str().unwrap_or("value"),
                    field.short.map_or_else(String::new, |s| format!(", -{s}")),
                    field
                        .default
                        .as_ref()
                        .map_or_else(String::new, |v| format!("; default={v}")),
                    field.doc
                );
            }
            let _ = writeln!(
                out,
                "\nStreams: stdin={}, stdout={}",
                command.stdin, command.stdout
            );
            let _ = writeln!(
                out,
                "Authorization: {}",
                if command.read_only {
                    "allow (read-only)"
                } else {
                    "confirm"
                }
            );
            if !command.errors.is_empty() {
                out.push_str("\nErrors:\n");
                for (name, code) in &command.errors {
                    let _ = writeln!(out, "  {name}: exit {code}");
                }
            }
            if !command.constraints.is_empty() {
                let _ = writeln!(
                    out,
                    "\nConstraints: {}",
                    Value::Array(command.constraints.clone())
                );
            }
            for section in ["result", "annotations"] {
                if !command.metadata[section].is_null() {
                    let _ = writeln!(out, "{section}: {}", command.metadata[section]);
                }
            }
        }
        if !node.children.is_empty() {
            out.push_str("\nCommands:\n");
            for child in &node.children {
                let child = &self.nodes[*child];
                let _ = writeln!(
                    out,
                    "  {} {} — {}",
                    child.name,
                    child.aliases.join(", "),
                    child.doc
                );
            }
        }
        out.push_str("\n  --help: show help at this command depth\n");
        out
    }

    fn find_path(&self, current: usize, wanted: usize, path: &mut Vec<String>) -> bool {
        if current == wanted {
            return true;
        }
        for child in &self.nodes[current].children {
            path.push(self.nodes[*child].name.clone());
            if self.find_path(*child, wanted, path) {
                return true;
            }
            path.pop();
        }
        false
    }
}

#[derive(Clone, Debug)]
pub struct ToolRequest {
    pub name: String,
    pub path: Vec<String>,
    /// A total canonical record, keyed by the authoritative field names.
    pub input: Value,
    pub stdin: Option<Vec<u8>>,
}

#[derive(Clone, Debug, Default)]
pub struct ToolOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: u8,
}

#[derive(Clone, Debug)]
pub struct ToolFailure {
    pub exit_code: u8,
    pub message: String,
}

impl ToolFailure {
    pub fn new(exit_code: u8, message: impl Into<String>) -> Self {
        Self {
            exit_code,
            message: message.into(),
        }
    }
}

/// Async transports supply a synchronous completion adapter using their own runtime.
#[async_trait::async_trait(?Send)]
pub trait ToolInvoker: Send + Sync {
    async fn invoke(&self, request: ToolRequest) -> Result<ToolOutput, ToolFailure>;
    /// Complete the invocation using the transport's runtime.
    ///
    /// # Errors
    /// Returns a transport or input failure with its shell exit status.
    fn invoke_blocking(&self, request: ToolRequest) -> Result<ToolOutput, ToolFailure>;
}

pub struct UnavailableToolInvoker;

#[async_trait::async_trait(?Send)]
impl ToolInvoker for UnavailableToolInvoker {
    async fn invoke(&self, request: ToolRequest) -> Result<ToolOutput, ToolFailure> {
        self.invoke_blocking(request)
    }

    fn invoke_blocking(&self, _request: ToolRequest) -> Result<ToolOutput, ToolFailure> {
        Err(ToolFailure::new(4, "agent tools need a Golem host"))
    }
}

pub struct ToolRuntime {
    pub definitions: BTreeMap<String, ToolDefinition>,
    pub shadowed: BTreeSet<String>,
    pub invoker: Arc<dyn ToolInvoker>,
}

impl ToolRuntime {
    /// Create a catalog of discovered tool commands.
    ///
    /// # Errors
    /// Rejects duplicate identities and malformed command trees.
    pub fn new(
        definitions: Vec<ToolDefinition>,
        invoker: Arc<dyn ToolInvoker>,
    ) -> Result<Self, String> {
        let mut by_name = BTreeMap::new();
        for definition in definitions {
            if definition.nodes.is_empty()
                || definition.name.is_empty()
                || definition.name.contains('/')
                || definition.name.contains("..")
                || definition.name.chars().any(char::is_whitespace)
            {
                return Err("invalid discovered tool name or empty command tree".into());
            }
            let mut seen = BTreeSet::new();
            if !visit_tree(&definition, 0, &mut seen) || seen.len() != definition.nodes.len() {
                return Err(
                    "invalid discovered command tree: cycle, shared child, or unreachable node"
                        .into(),
                );
            }
            if by_name
                .insert(definition.name.clone(), definition)
                .is_some()
            {
                return Err("duplicate discovered tool lookup name".into());
            }
        }
        Ok(Self {
            definitions: by_name,
            shadowed: BTreeSet::new(),
            invoker,
        })
    }

    #[must_use]
    pub fn policy(
        &self,
        name: &str,
        words: &[String],
    ) -> Option<crate::manifest::AuthorizationPolicy> {
        use crate::manifest::AuthorizationPolicy::{Allow, Confirm};
        let definition = self.definitions.get(name)?;
        if self.shadowed.contains(name) {
            return None;
        }
        Some(match parse(definition, words, |_| None) {
            Ok(Projection::Help(_)) | Err(_) => Allow,
            Ok(Projection::Call { command, .. }) if command.read_only => Allow,
            Ok(_) => Confirm,
        })
    }
}

fn visit_tree(definition: &ToolDefinition, index: usize, seen: &mut BTreeSet<usize>) -> bool {
    seen.insert(index)
        && definition.nodes.get(index).is_some_and(|node| {
            node.children
                .iter()
                .all(|child| visit_tree(definition, *child, seen))
        })
}
