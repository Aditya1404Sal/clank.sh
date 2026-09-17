//! Shared Golem discovery and invocation adapter for bash-tool and embedded shells.
#![allow(missing_docs)]
use bash::agent_tools::{
    Argument, ToolCommand, ToolDefinition, ToolFailure, ToolNode, ToolRequest,
};
use golem_rust::schema::{
    render::json_value::{from_untrusted_json_value, to_json_value},
    tool::{canonical::CanonicalSurfaceRef, Tool},
    SchemaType,
};
use serde_json::{json, Value};

/// Project validated SDK metadata into the shell's transport-independent catalog.
///
/// # Errors
/// Rejects malformed metadata and invalid canonical defaults.
#[allow(
    clippy::too_many_lines,
    reason = "mapping canonical surfaces is kept in descriptor order"
)]
pub fn project(name: String, tool: &Tool) -> Result<ToolDefinition, String> {
    golem_rust::schema::tool::validation::validate_tool(tool).map_err(|e| format!("{e:?}"))?;
    let raw = serde_json::to_value(tool).map_err(|e| e.to_string())?;
    let mut nodes = Vec::new();
    for (index, node) in tool.commands.nodes.iter().enumerate() {
        let mut globals = Vec::new();
        for (i, _) in node.globals.options.iter().enumerate() {
            let surface = CanonicalSurfaceRef::GlobalOption {
                node: index,
                index: i,
            };
            let field = tool
                .canonical_field_for_surface(index, surface)
                .ok_or("missing global option")?;
            globals.push(argument(
                &raw["commands"]["nodes"][index]["globals"]["options"][i],
                &field,
                &tool.schema,
                false,
            )?);
        }
        for (i, _) in node.globals.flags.iter().enumerate() {
            let field = tool
                .canonical_field_for_surface(
                    index,
                    CanonicalSurfaceRef::GlobalFlag {
                        node: index,
                        index: i,
                    },
                )
                .ok_or("missing global flag")?;
            globals.push(argument(
                &raw["commands"]["nodes"][index]["globals"]["flags"][i],
                &field,
                &tool.schema,
                true,
            )?);
        }
        let command = if let Some(body) = &node.body {
            let model = tool
                .canonical_input_model(index)
                .map_err(|e| format!("{e:?}"))?;
            let mut fields = Vec::new();
            for (surface, field) in tool
                .canonical_input_surfaces(index)
                .into_iter()
                .zip(&model.fields)
            {
                let n = &raw["commands"]["nodes"];
                let b = &n[index]["body"];
                let (value, kind) = match surface {
                    CanonicalSurfaceRef::GlobalOption { node, index } => {
                        (&n[node]["globals"]["options"][index], "option")
                    }
                    CanonicalSurfaceRef::GlobalFlag { node, index } => {
                        (&n[node]["globals"]["flags"][index], "flag")
                    }
                    CanonicalSurfaceRef::BodyOption { index } => (&b["options"][index], "option"),
                    CanonicalSurfaceRef::BodyFlag { index } => (&b["flags"][index], "flag"),
                    CanonicalSurfaceRef::BodyPositional { index } => {
                        (&b["positionals"]["fixed"][index], "positional")
                    }
                    CanonicalSurfaceRef::BodyTail => (&b["positionals"]["tail"], "tail"),
                };
                let mut arg = argument(value, field, &model.record_schema, kind == "flag")?;
                if matches!(kind, "positional" | "tail") {
                    arg.kind = kind.into();
                    arg.spec = value.clone();
                }
                fields.push(arg);
            }
            let mut path = Vec::new();
            if !path_to(tool, 0, index, &mut path) {
                return Err("unreachable command".into());
            }
            let mut constraints = raw["commands"]["nodes"][index]["body"]["constraints"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            // Constraint literals are SchemaValue on the wire; compare ordinary JSON in the pure parser.
            for constraint in &mut constraints {
                normalize(constraint, &fields, &model.record_schema)?;
            }
            Some(ToolCommand {
                path,
                fields,
                schema: serde_json::to_value(&model.record_schema).map_err(|e| e.to_string())?,
                constraints,
                read_only: body.annotations.is_some_and(|a| a.read_only),
                stdin: body.stdin.is_some(),
                stdout: body.stdout.is_some(),
                errors: body
                    .errors
                    .iter()
                    .map(|e| (e.name.clone(), e.exit_code))
                    .collect(),
                metadata: raw["commands"]["nodes"][index]["body"].clone(),
            })
        } else {
            None
        };
        nodes.push(ToolNode {
            name: node.name.clone(),
            aliases: node.aliases.clone(),
            doc: doc(&raw["commands"]["nodes"][index]["doc"]),
            children: node
                .subcommands
                .iter()
                .filter_map(|i| i.as_usize())
                .collect(),
            globals,
            command,
        });
    }
    Ok(ToolDefinition {
        name,
        version: tool.version.clone(),
        nodes,
    })
}
fn doc(value: &Value) -> String {
    let mut text = value["summary"].as_str().unwrap_or_default().to_owned();
    if let Some(description) = value["description"].as_str() {
        text.push('\n');
        text.push_str(description);
    }
    if let Some(examples) = value["examples"].as_array() {
        for example in examples {
            text.push('\n');
            text.push_str(&example.to_string());
        }
    }
    text
}
fn argument(
    value: &Value,
    field: &golem_rust::schema::tool::canonical::CanonicalInputField,
    graph: &golem_rust::schema::SchemaGraph,
    flag: bool,
) -> Result<Argument, String> {
    let shape = &value["shape"];
    let kind = shape["kind"].as_str().unwrap_or("scalar");
    let mut spec = shape["value"].clone();
    if kind == "count-flag" {
        spec = json!({"max": spec});
    }
    let default = if flag {
        match kind {
            "bool-flag" => Some(shape["value"]["default"].clone()),
            "count-flag" => Some(json!(0)),
            _ => None,
        }
    } else if !value["default"].is_null() {
        let default =
            serde_json::from_value(value["default"].clone()).map_err(|e| e.to_string())?;
        Some(to_json_value(graph, &field.type_, &default).map_err(|e| format!("{e:?}"))?)
    } else {
        None
    };
    Ok(Argument {
        name: field.name.clone(),
        aliases: field.aliases.clone(),
        short: field.short,
        kind: kind.into(),
        schema: serde_json::to_value(&field.type_).map_err(|e| e.to_string())?,
        spec,
        default,
        env_var: value["env_var"].as_str().map(str::to_owned),
        required: value["required"].as_bool().unwrap_or(false),
        doc: doc(&value["doc"]),
    })
}

/// Construct the exact published canonical input model from untrusted shell arguments.
///
/// # Errors
/// Rejects unknown command paths, invalid input, and host capability construction.
pub fn encode_input(
    tool: &Tool,
    request: &ToolRequest,
) -> Result<golem_rust::TypedSchemaValue, ToolFailure> {
    let index = tool
        .command_index_by_path(&request.path)
        .ok_or_else(|| ToolFailure::new(2, "unknown command path"))?;
    let model = tool
        .canonical_input_model(index)
        .map_err(|e| ToolFailure::new(2, format!("{e:?}")))?;
    let value = from_untrusted_json_value(
        &model.record_schema,
        &model.record_schema.root,
        &request.input,
    )
    .map_err(|e| ToolFailure::new(2, format!("invalid input: {e:?}")))?;
    golem_rust::schema::validation::value::validate_value(
        &model.record_schema,
        &model.record_schema.root,
        &value,
    )
    .map_err(|e| ToolFailure::new(2, format!("invalid input: {e:?}")))?;
    Ok(golem_rust::TypedSchemaValue::new(
        model.record_schema,
        value,
    ))
}

#[cfg(target_arch = "wasm32")]
mod transport;
#[cfg(target_arch = "wasm32")]
pub use transport::{discover, GolemToolInvoker};

/// Bind the current owner's tools. Host imports are used only on wasm.
///
/// # Errors
/// Returns an error when host discovery contains invalid metadata.
pub fn install(session: &mut bash::session::Session) -> Result<(), String> {
    #[cfg(target_arch = "wasm32")]
    {
        session.set_tools(discover()?);
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = session;
    }
    Ok(())
}

fn path_to(tool: &Tool, current: usize, wanted: usize, path: &mut Vec<String>) -> bool {
    if current == wanted {
        return true;
    }
    for child in &tool.commands.nodes[current].subcommands {
        let Some(child) = child.as_usize() else {
            continue;
        };
        path.push(tool.commands.nodes[child].name.clone());
        if path_to(tool, child, wanted, path) {
            return true;
        }
        path.pop();
    }
    false
}

fn normalize(
    v: &mut Value,
    fields: &[Argument],
    graph: &golem_rust::schema::SchemaGraph,
) -> Result<(), String> {
    if v["kind"] == "value-is" {
        let name = v["value"]["name"].as_str().ok_or("constraint name")?;
        let field = fields
            .iter()
            .find(|f| f.name == name || f.aliases.iter().any(|a| a == name))
            .ok_or("constraint field")?;
        let ty: SchemaType =
            serde_json::from_value(field.schema.clone()).map_err(|e| e.to_string())?;
        let value =
            serde_json::from_value(v["value"]["value"].clone()).map_err(|e| e.to_string())?;
        v["value"]["name"] = field.name.clone().into();
        v["value"]["value"] = to_json_value(graph, &ty, &value).map_err(|e| format!("{e:?}"))?;
    } else if v["kind"] == "present" {
        let name = v["value"].as_str().ok_or("constraint name")?;
        let field = fields
            .iter()
            .find(|f| f.name == name || f.aliases.iter().any(|a| a == name))
            .ok_or("constraint field")?;
        v["value"] = field.name.clone().into();
    } else if let Some(items) = v.as_array_mut() {
        for item in items {
            normalize(item, fields, graph)?;
        }
    } else if let Some(object) = v.as_object_mut() {
        for item in object.values_mut() {
            normalize(item, fields, graph)?;
        }
    }
    Ok(())
}
