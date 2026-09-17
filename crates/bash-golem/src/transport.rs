use bash::agent_tools::{
    ToolFailure, ToolInvoker, ToolOutput, ToolRequest, ToolRuntime, MAX_ATTACHMENT_BYTES,
};
use golem_rust::{
    bindings::golem::tool::host,
    schema::{
        render::json_value::to_json_value_redacted,
        tool::{wit::decode_tool, Tool},
    },
};
use std::{collections::BTreeMap, sync::Arc};

pub struct GolemToolInvoker {
    tools: BTreeMap<String, Tool>,
}

/// Discover exactly the owner's bound tool lookup identities.
///
/// # Errors
/// Rejects invalid discovered metadata.
pub fn discover() -> Result<ToolRuntime, String> {
    let mut definitions = Vec::new();
    let mut tools = BTreeMap::new();
    for registered in host::get_all_tools() {
        let tool =
            decode_tool(registered.definition).map_err(|e| format!("tool discovery: {e:?}"))?;
        definitions.push(super::project(registered.lookup_name.clone(), &tool)?);
        tools.insert(registered.lookup_name, tool);
    }
    ToolRuntime::new(definitions, Arc::new(GolemToolInvoker { tools }))
}

#[async_trait::async_trait(?Send)]
impl ToolInvoker for GolemToolInvoker {
    fn invoke_blocking(&self, request: ToolRequest) -> Result<ToolOutput, ToolFailure> {
        wit_bindgen::block_on(self.invoke(request))
    }

    #[allow(
        clippy::too_many_lines,
        reason = "attachment pumping and terminal arbitration form one invocation"
    )]
    async fn invoke(&self, request: ToolRequest) -> Result<ToolOutput, ToolFailure> {
        let tool = self
            .tools
            .get(&request.name)
            .ok_or_else(|| ToolFailure::new(127, "tool is not bound"))?;
        let index = tool
            .command_index_by_path(&request.path)
            .ok_or_else(|| ToolFailure::new(2, "unknown command"))?;
        let body = tool.commands.nodes[index]
            .body
            .as_ref()
            .ok_or_else(|| ToolFailure::new(2, "no command body"))?;
        let typed = super::encode_input(tool, &request)?;
        let input = golem_rust::encode_typed_schema_value(&typed)
            .map_err(|e| ToolFailure::new(2, format!("{e:?}")))?;
        let rpc = host::ToolRpc::new(&request.name);
        let (writer, stdin) = match request.stdin {
            Some(bytes) => {
                if bytes.len() > MAX_ATTACHMENT_BYTES {
                    return Err(ToolFailure::new(1, "stdin attachment limit exceeded"));
                }
                let (writer, source, watcher) = host::create_stdin();
                // No native read is outstanding: the finite pump owns the bytes.
                drop(watcher);
                (Some((writer, bytes)), Some(source))
            }
            None => (None, None),
        };
        let (stdout, reader) = if body.stdout.is_some() {
            let (target, reader) = host::create_stdout();
            (Some(target), Some(reader))
        } else {
            (None, None)
        };
        let pump = async {
            if let Some((writer, bytes)) = writer {
                for chunk in bytes.chunks(64 * 1024) {
                    writer
                        .write(chunk.to_vec())
                        .await
                        .map_err(|e| format!("stdin: {e:?}"))?;
                }
                writer.finish().await.map_err(|e| format!("stdin: {e:?}"))?;
            }
            Ok::<_, String>(())
        };
        let drain = async {
            let mut bytes = Vec::new();
            let mut failure = None;
            if let Some(mut reader) = reader {
                while let Some(item) = reader.next().await {
                    let chunk = match item {
                        Ok(chunk) => chunk,
                        Err(error) => {
                            failure = Some(format!("stdout: {error:?}"));
                            break;
                        }
                    };
                    let remaining = MAX_ATTACHMENT_BYTES - bytes.len();
                    bytes.extend(&chunk[..chunk.len().min(remaining)]);
                    if chunk.len() > remaining {
                        failure = Some("stdout attachment limit exceeded".to_string());
                        break;
                    }
                }
            }
            (bytes, failure)
        };
        let (terminal, pumped, (stdout, drain_failure)) = futures::join!(
            rpc.invoke_and_await(request.path, input, stdin, stdout),
            pump,
            drain
        );
        let mut output = ToolOutput {
            stdout,
            ..Default::default()
        };
        if let Some(error) = drain_failure {
            output.stderr = format!("{error}\n").into_bytes();
            output.exit_code = 1;
        }
        match terminal {
            Ok(result) => {
                // A declared stdout is authoritative even when the stream is empty.
                if body.stdout.is_none() {
                    if let Some(wire) = result.result {
                        let typed = golem_rust::decode_typed_schema_value(&wire)
                            .map_err(|e| ToolFailure::new(1, format!("result: {e:?}")))?;
                        let json = to_json_value_redacted(
                            typed.graph(),
                            &typed.graph().root,
                            typed.value(),
                        )
                        .map_err(|e| ToolFailure::new(1, format!("result: {e:?}")))?;
                        output.stdout = match json {
                            serde_json::Value::String(text) => text.into_bytes(),
                            value => format!("{value}\n").into_bytes(),
                        };
                    }
                }
                if let Err(e) = pumped {
                    output.stderr.extend(format!("{e}\n").bytes());
                    output.exit_code = 1;
                }
            }
            Err(error) => {
                output.exit_code = match &error {
                    host::RpcError::Denied(_) => 3,
                    host::RpcError::NotFound(_) => 127,
                    host::RpcError::Cancelled => 130,
                    host::RpcError::RemoteToolError(
                        host::ToolError::InvalidInput(_) | host::ToolError::ConstraintViolation(_),
                    ) => 2,
                    host::RpcError::RemoteToolError(host::ToolError::CustomError(custom)) => body
                        .errors
                        .iter()
                        .find(|e| e.name == custom.name)
                        .map_or(1, |e| e.exit_code),
                    _ => 1,
                };
                // Never debug-print capability-bearing error payloads.
                let message = match error {
                    host::RpcError::RemoteToolError(host::ToolError::CustomError(custom)) => {
                        let payload = golem_rust::decode_typed_schema_value(&custom.payload)
                            .ok()
                            .and_then(|typed| {
                                to_json_value_redacted(
                                    typed.graph(),
                                    &typed.graph().root,
                                    typed.value(),
                                )
                                .ok()
                            });
                        match payload {
                            Some(payload) => format!("tool error: {}: {payload}", custom.name),
                            None => format!("tool error: {}", custom.name),
                        }
                    }
                    host::RpcError::Denied(message)
                    | host::RpcError::NotFound(message)
                    | host::RpcError::ProtocolError(message)
                    | host::RpcError::RemoteInternalError(message)
                    | host::RpcError::ResourceExhausted(message)
                    | host::RpcError::RemoteToolError(
                        host::ToolError::InvalidInput(message)
                        | host::ToolError::ConstraintViolation(message),
                    ) => message,
                    other => format!("{other:?}"),
                };
                output.stderr.extend(format!("{message}\n").bytes());
            }
        }
        Ok(output)
    }
}
