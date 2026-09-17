use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Map, Value};

use super::{Argument, ToolCommand, ToolDefinition};

#[derive(Clone, Debug)]
pub enum Projection {
    Help(String),
    Call { command: ToolCommand, input: Value },
}

/// Parse already-expanded argv. Presence is retained separately from the total wire record.
///
/// # Errors
/// Rejects unknown switches, missing/invalid arguments, and violated constraints.
#[allow(
    clippy::too_many_lines,
    reason = "one cursor implements the CLI grammar across command depths"
)]
pub fn parse(
    tool: &ToolDefinition,
    words: &[String],
    env: impl Fn(&str) -> Option<String>,
) -> Result<Projection, String> {
    let mut index = 0;
    let mut globals = tool.nodes[0].globals.clone();
    let mut supplied: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut positionals = Vec::new();
    let mut after_separator = false;
    let mut separator_seen = false;
    let mut cursor = 0;
    while cursor < words.len() {
        let word = &words[cursor];
        let node = &tool.nodes[index];
        if !after_separator && word == "--help" {
            return Ok(Projection::Help(tool.help(index)));
        }
        let fields: Vec<&Argument> = globals
            .iter()
            .chain(node.command.iter().flat_map(|c| {
                c.fields
                    .iter()
                    .filter(|f| !globals.iter().any(|g| g.name == f.name))
            }))
            .collect();
        let separator = node
            .command
            .as_ref()
            .and_then(|c| c.fields.iter().find(|f| f.kind == "tail"))
            .and_then(|f| f.spec["separator"].as_str());
        if !after_separator && (word == "--" || separator == Some(word.as_str())) {
            separator_seen = true;
            after_separator = true;
            cursor += 1;
            continue;
        }
        if !after_separator && word.starts_with("--") {
            let (name, attached) = word[2..]
                .split_once('=')
                .map_or((&word[2..], None), |(n, v)| (n, Some(v)));
            let negated = name.strip_prefix("no-");
            let direct = fields
                .iter()
                .find(|f| f.name == name || f.aliases.iter().any(|a| a == name));
            let field = direct
                .or_else(|| {
                    negated.and_then(|name| {
                        fields.iter().find(|f| {
                            (f.name == name || f.aliases.iter().any(|a| a == name))
                                && f.kind == "bool-flag"
                                && f.spec["negatable"] == true
                        })
                    })
                })
                .ok_or_else(|| format!("unknown option --{name}"))?;
            let value = option_value(
                field,
                attached,
                words,
                &mut cursor,
                direct.is_none() && negated.is_some(),
            )?;
            supplied.entry(field.name.clone()).or_default().push(value);
        } else if !after_separator && word.starts_with('-') && word != "-" {
            for (offset, name) in word[1..].char_indices() {
                let field = fields
                    .iter()
                    .find(|f| f.short == Some(name))
                    .ok_or_else(|| format!("unknown option -{name}"))?;
                let takes_value = !matches!(field.kind.as_str(), "bool-flag" | "count-flag");
                let remainder = &word[1 + offset + name.len_utf8()..];
                let attached = (takes_value && !remainder.is_empty()).then_some(remainder);
                let value = option_value(field, attached, words, &mut cursor, false)?;
                supplied.entry(field.name.clone()).or_default().push(value);
                if takes_value {
                    break;
                }
            }
        } else if !after_separator && positionals.is_empty() {
            if let Some(child) = node.children.iter().find(|child| {
                let child = &tool.nodes[**child];
                child.name == *word || child.aliases.contains(word)
            }) {
                index = *child;
                globals.extend(tool.nodes[index].globals.clone());
            } else if node.command.is_some() {
                positionals.push(word.clone());
            } else {
                return Err(format!("unknown command {word}; use {} --help", tool.name));
            }
        } else {
            positionals.push(word.clone());
        }
        cursor += 1;
    }
    let command = tool.nodes[index]
        .command
        .as_ref()
        .ok_or_else(|| format!("a subcommand is required; use {} --help", tool.name))?;
    let mut input = Map::new();
    let mut present = BTreeSet::new();
    let mut position = 0;
    for field in &command.fields {
        let values = match field.kind.as_str() {
            "positional" => {
                let value = positionals.get(position).cloned();
                if value.is_some() {
                    position += 1;
                }
                value.map(|v| vec![v])
            }
            "tail" => {
                let values = positionals[position..].to_vec();
                position = positionals.len();
                if !values.is_empty() && !field.spec["separator"].is_null() && !separator_seen {
                    return Err(format!(
                        "{} requires separator {}",
                        field.name, field.spec["separator"]
                    ));
                }
                let count = values.len() as u64;
                if count < field.spec["min"].as_u64().unwrap_or(0)
                    || field.spec["max"].as_u64().is_some_and(|max| count > max)
                {
                    return Err(format!("invalid number of {} arguments", field.name));
                }
                (!values.is_empty()).then_some(values)
            }
            _ => supplied.remove(&field.name),
        };
        let from_env = values
            .is_none()
            .then(|| field.env_var.as_deref().and_then(&env))
            .flatten();
        let values = values.or_else(|| from_env.map(|v| vec![v]));
        let value = if let Some(values) = values {
            present.insert(field.name.clone());
            collect(field, &values, &command.schema)?
        } else if let Some(default) = &field.default {
            default.clone()
        } else if field.required {
            return Err(format!("missing required argument {}", field.name));
        } else {
            empty_value(&field.schema, &command.schema, 0)?
        };
        input.insert(field.name.clone(), value);
    }
    if position != positionals.len() {
        return Err(format!("unexpected argument {}", positionals[position]));
    }
    check_constraints(&command.constraints, &present, &input)?;
    Ok(Projection::Call {
        command: command.clone(),
        input: Value::Object(input),
    })
}

fn option_value(
    field: &Argument,
    attached: Option<&str>,
    words: &[String],
    cursor: &mut usize,
    negated: bool,
) -> Result<String, String> {
    match field.kind.as_str() {
        "bool-flag" | "count-flag" => {
            if attached.is_some() {
                return Err(format!("--{} does not take a value", field.name));
            }
            Ok(if negated { "false" } else { "true" }.into())
        }
        "optional-scalar"
            if attached.is_none() && words.get(*cursor + 1).is_none_or(|w| w.starts_with('-')) =>
        {
            field
                .default
                .as_ref()
                .map(|_| String::new())
                .ok_or_else(|| format!("--{} needs a value or declared default", field.name))
        }
        _ => attached.map(str::to_owned).map_or_else(
            || {
                *cursor += 1;
                words
                    .get(*cursor)
                    .cloned()
                    .ok_or_else(|| format!("--{} needs a value", field.name))
            },
            Ok,
        ),
    }
}

fn collect(field: &Argument, values: &[String], graph: &Value) -> Result<Value, String> {
    let error = |message: String| format!("{}: {message}", field.name);
    match field.kind.as_str() {
        "bool-flag" => Ok(json!(values.last().is_some_and(|v| v == "true"))),
        "count-flag" => {
            let count = values.len() as u64;
            if field.spec["max"].as_u64().is_some_and(|max| count > max) {
                return Err(error("flag count exceeds maximum".into()));
            }
            Ok(json!(count))
        }
        "repeatable-list" | "repeatable-map" | "tail" => {
            let repetition = &field.spec["repetition"];
            if repetition["kind"] == "delimited" && values.len() > 1 {
                return Err(error("option may occur only once".into()));
            }
            let delimiter = match repetition["kind"].as_str() {
                Some("either" | "delimited") => {
                    repetition["value"].as_str().and_then(|s| s.chars().next())
                }
                _ => None,
            };
            let parts: Vec<&str> = values
                .iter()
                .flat_map(|v| delimiter.map_or_else(|| vec![v.as_str()], |d| v.split(d).collect()))
                .collect();
            let schema = resolve(&field.schema, graph, 0)?;
            if field.kind == "repeatable-map" {
                let mut map = Map::new();
                for part in parts {
                    let (key, value) = part
                        .split_once('=')
                        .ok_or_else(|| error("expected key=value".into()))?;
                    let key_value =
                        coerce(&schema["value"]["key"], key, graph, 0).map_err(&error)?;
                    let key = key_value
                        .as_str()
                        .map_or_else(|| key_value.to_string(), str::to_owned);
                    if map.contains_key(&key) && field.spec["duplicate_key_policy"] != "last-wins" {
                        return Err(error(format!("duplicate key {key}")));
                    }
                    map.insert(
                        key,
                        coerce(&schema["value"]["value"], value, graph, 0).map_err(&error)?,
                    );
                }
                Ok(Value::Array(
                    map.into_iter()
                        .map(|(key, value)| {
                            let key = coerce(&schema["value"]["key"], &key, graph, 0)
                                .unwrap_or(Value::String(key));
                            json!([key, value])
                        })
                        .collect(),
                ))
            } else {
                parts
                    .into_iter()
                    .map(|v| coerce(&schema["value"]["element"], v, graph, 0).map_err(&error))
                    .collect::<Result<Vec<_>, _>>()
                    .map(Value::Array)
            }
        }
        "optional-scalar" if values.last().is_some_and(String::is_empty) => {
            Ok(field.default.clone().unwrap_or(Value::Null))
        }
        _ => {
            if values.len() > 1 {
                return Err(error("option supplied more than once".into()));
            }
            coerce(&field.schema, &values[0], graph, 0).map_err(error)
        }
    }
}

fn resolve<'a>(schema: &'a Value, graph: &'a Value, depth: usize) -> Result<&'a Value, String> {
    if depth > 64 {
        return Err("schema nesting exceeds 64".into());
    }
    if schema["kind"] == "ref" {
        let id = schema["value"]["id"]
            .as_str()
            .ok_or("missing schema reference identity")?;
        let body = graph["defs"]
            .as_array()
            .and_then(|defs| defs.iter().find(|d| d["id"] == id))
            .map(|d| &d["body"])
            .ok_or_else(|| format!("unknown schema reference {id}"))?;
        resolve(body, graph, depth + 1)
    } else {
        Ok(schema)
    }
}

fn coerce(schema: &Value, text: &str, graph: &Value, depth: usize) -> Result<Value, String> {
    let schema = resolve(schema, graph, depth)?;
    let kind = schema["kind"].as_str().ok_or("missing schema kind")?;
    match kind {
        "secret" | "quota-token" => Err(format!(
            "{kind} capability arguments cannot be supplied from text"
        )),
        "option" => {
            if text == "null" {
                Ok(Value::Null)
            } else {
                coerce(&schema["value"]["inner"], text, graph, depth + 1)
            }
        }
        "string" | "path" | "url" | "datetime" | "date-time" | "duration" | "enum" => {
            Ok(json!(text))
        }
        "bool" => match text {
            "true" | "1" | "yes" => Ok(json!(true)),
            "false" | "0" | "no" => Ok(json!(false)),
            _ => Err("expected a boolean".into()),
        },
        "u8" | "u16" | "u32" | "u64" => {
            let value = text
                .parse::<u64>()
                .map_err(|_| format!("expected {kind}"))?;
            let bits = kind[1..]
                .parse::<u32>()
                .map_err(|_| "invalid integer width")?;
            if bits < 64 && value >= (1u64 << bits) {
                return Err(format!("value is outside {kind}"));
            }
            Ok(json!(value))
        }
        "s8" | "s16" | "s32" | "s64" => {
            let value = text
                .parse::<i64>()
                .map_err(|_| format!("expected {kind}"))?;
            let bits = kind[1..]
                .parse::<u32>()
                .map_err(|_| "invalid integer width")?;
            if bits < 64 && (value < -(1i64 << (bits - 1)) || value >= (1i64 << (bits - 1))) {
                return Err(format!("value is outside {kind}"));
            }
            Ok(json!(value))
        }
        "f32" | "f64" => {
            let value = text
                .parse::<f64>()
                .map_err(|_| format!("expected {kind}"))?;
            if !value.is_finite() {
                return Err("expected a finite number".into());
            }
            Ok(json!(value))
        }
        _ => serde_json::from_str(text).map_err(|e| format!("expected a {kind} JSON literal: {e}")),
    }
}

fn empty_value(schema: &Value, graph: &Value, depth: usize) -> Result<Value, String> {
    let schema = resolve(schema, graph, depth)?;
    match schema["kind"].as_str() {
        Some("secret" | "quota-token") => {
            Err("capability arguments cannot be supplied from text".into())
        }
        Some("option") => Ok(Value::Null),
        Some("bool") => Ok(json!(false)),
        Some("list" | "map") => Ok(json!([])),
        Some("tuple") => schema["value"]["items"]
            .as_array()
            .ok_or("missing tuple members")?
            .iter()
            .map(|item| empty_value(item, graph, depth + 1))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        Some("record") => {
            let mut object = Map::new();
            for field in schema["value"]["fields"]
                .as_array()
                .ok_or("missing record fields")?
            {
                let name = field["name"].as_str().ok_or("missing record field name")?;
                object.insert(name.into(), empty_value(&field["body"], graph, depth + 1)?);
            }
            Ok(Value::Object(object))
        }
        Some(kind)
            if kind.starts_with('u') && kind[1..].parse::<u32>().is_ok()
                || kind.starts_with('s') && kind[1..].parse::<u32>().is_ok()
                || matches!(kind, "f32" | "f64") =>
        {
            Ok(json!(0))
        }
        Some("string") => Ok(json!("")),
        Some(kind) => Err(format!("argument of type {kind} needs a value or default")),
        None => Err("missing schema type".into()),
    }
}

fn check_constraints(
    constraints: &[Value],
    present: &BTreeSet<String>,
    input: &Map<String, Value>,
) -> Result<(), String> {
    fn matches(reference: &Value, present: &BTreeSet<String>, input: &Map<String, Value>) -> bool {
        if reference["kind"] == "present" {
            reference["value"]
                .as_str()
                .is_some_and(|name| present.contains(name))
        } else if reference["kind"] == "value-is" {
            reference["value"]["name"]
                .as_str()
                .and_then(|name| input.get(name))
                .is_some_and(|value| *value == reference["value"]["value"])
        } else {
            false
        }
    }
    let eval = |refs: &Value, all: bool| {
        let refs = refs.as_array().map_or(&[][..], Vec::as_slice);
        if all {
            refs.iter().all(|r| matches(r, present, input))
        } else {
            refs.iter().any(|r| matches(r, present, input))
        }
    };
    for constraint in constraints {
        let value = &constraint["value"];
        let valid = match constraint["kind"].as_str() {
            Some("requires-all") => eval(value, true),
            Some("requires-any") => eval(value, false),
            Some("all-or-none") => !eval(value, false) || eval(value, true),
            Some("mutex-groups") => {
                value.as_array().map_or(0, |groups| {
                    groups.iter().filter(|g| eval(&g["refs"], false)).count()
                }) <= 1
            }
            Some("implies") => {
                !eval(&value["lhs"], value["lhs_quant"] == "all")
                    || eval(&value["rhs"], value["rhs_quant"] == "all")
            }
            Some("forbids") => {
                !eval(&value["lhs"], value["lhs_quant"] == "all") || !eval(&value["rhs"], false)
            }
            _ => return Err("unsupported tool constraint".into()),
        };
        if !valid {
            return Err(format!("argument constraint violated: {constraint}"));
        }
    }
    Ok(())
}
