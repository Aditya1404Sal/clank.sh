use super::{parse, Projection, ToolRequest, ToolRuntime, MAX_ATTACHMENT_BYTES};
use brush_core::{
    builtins::{ContentOptions, ContentType, SimpleCommand},
    commands::ExecutionContext,
    extensions::ShellExtensions,
    Error, ExecutionResult,
};
use std::{
    cell::RefCell,
    collections::BTreeSet,
    io::{Read, Write},
    sync::Arc,
};

#[derive(Clone)]
struct Context {
    runtime: Arc<ToolRuntime>,
    approved: BTreeSet<(String, Vec<String>)>,
}
thread_local! {
    static CURRENT: RefCell<Option<Context>> = const { RefCell::new(None) };
}
pub(crate) struct InstallGuard(Option<Context>);
pub(crate) fn suspend() -> InstallGuard {
    InstallGuard(CURRENT.with(|slot| slot.replace(None)))
}
impl Drop for InstallGuard {
    fn drop(&mut self) {
        CURRENT.with(|slot| *slot.borrow_mut() = self.0.take());
    }
}
pub(crate) fn words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut last_word_end = None;
    let mut tokens = brush_parser::tokenize_str(line)
        .unwrap_or_default()
        .into_iter()
        .peekable();
    while let Some(token) = tokens.next() {
        match token {
            brush_parser::Token::Word(s, location) => {
                words.push(brush_parser::unquote_str(&s));
                last_word_end = Some(location.end.index);
            }
            brush_parser::Token::Operator(operator, location)
                if matches!(
                    operator.as_str(),
                    "<" | ">"
                        | ">>"
                        | ">|"
                        | "<>"
                        | "<&"
                        | ">&"
                        | "&>"
                        | "&>>"
                        | "<<"
                        | "<<-"
                        | "<<<"
                ) =>
            {
                if last_word_end == Some(location.start.index)
                    && words
                        .last()
                        .is_some_and(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()))
                {
                    words.pop();
                }
                tokens.next();
                last_word_end = None;
            }
            brush_parser::Token::Operator(..) => {}
        }
    }
    words
}
pub(crate) fn install(runtime: Arc<ToolRuntime>, line: &str) -> InstallGuard {
    let mut approved = BTreeSet::new();
    for segment in crate::authz::split_segments(line) {
        let mut argv = words(segment);
        if argv.first().is_some_and(|s| s == "sudo") {
            argv.remove(0);
        }
        if let Some(definition) = argv.first().and_then(|n| runtime.definitions.get(n)) {
            if let Ok(Projection::Call { command, .. }) = parse(definition, &argv[1..], |_| None) {
                approved.insert((definition.name.clone(), command.path));
            }
        }
    }
    let previous = CURRENT.with(|slot| slot.replace(Some(Context { runtime, approved })));
    InstallGuard(previous)
}
struct ToolBuiltin;
pub(crate) fn registration(
) -> brush_core::builtins::Registration<brush_core::extensions::DefaultShellExtensions> {
    brush_core::builtins::simple_builtin::<
        ToolBuiltin,
        brush_core::extensions::DefaultShellExtensions,
    >()
}
impl SimpleCommand for ToolBuiltin {
    fn get_content(name: &str, _: ContentType, _: &ContentOptions) -> Result<String, Error> {
        Ok(CURRENT
            .with(|s| {
                s.borrow()
                    .as_ref()
                    .and_then(|c| c.runtime.definitions.get(name))
                    .map(|d| d.help(0))
            })
            .unwrap_or_else(|| format!("{name}: bound agent tool\n")))
    }
    fn execute<SE: ShellExtensions, I: Iterator<Item = S>, S: AsRef<str>>(
        context: ExecutionContext<'_, SE>,
        arguments: I,
    ) -> Result<ExecutionResult, Error> {
        let argv: Vec<String> = arguments.map(|s| s.as_ref().to_owned()).collect();
        // Release the TLS borrow before entering the transport's async runtime.
        let current = CURRENT.with(|slot| slot.borrow().clone());
        let result = (|| {
            let current = current.ok_or_else(|| {
                super::ToolFailure::new(4, "agent tools need a Golem host context")
            })?;
            let definition = current
                .runtime
                .definitions
                .get(&context.command_name)
                .ok_or_else(|| super::ToolFailure::new(127, "tool is not bound"))?;
            let projection = parse(definition, &argv[1..], |name| {
                context
                    .shell
                    .env()
                    .get(name)
                    .map(|v| v.1.value().to_cow_str(context.shell).to_string())
            })
            .map_err(|e| super::ToolFailure::new(2, e))?;
            match projection {
                Projection::Help(help) => Ok(super::ToolOutput {
                    stdout: help.into_bytes(),
                    ..Default::default()
                }),
                Projection::Call { command, input } => {
                    // Nested functions/eval/source/substitutions cannot inherit a visible command's grant.
                    if !command.read_only
                        && (context.shell.call_stack().in_function()
                            || context.shell.call_stack().depth() > 1
                            || !current
                                .approved
                                .contains(&(definition.name.clone(), command.path.clone())))
                    {
                        crate::logging::Record::new("tool-denied")
                            .field("tool", &definition.name)
                            .field("path", command.path.join("/"))
                            .field("exit", "3")
                            .emit(crate::logging::LogFile::Rpc);
                        return Err(super::ToolFailure::new(3, "confirmation cannot be requested here; invoke this tool command directly"));
                    }
                    let stdin = if command.stdin {
                        let mut bytes = Vec::new();
                        context
                            .stdin()
                            .take((MAX_ATTACHMENT_BYTES + 1) as u64)
                            .read_to_end(&mut bytes)
                            .map_err(|e| super::ToolFailure::new(1, format!("stdin: {e}")))?;
                        if bytes.len() > MAX_ATTACHMENT_BYTES {
                            return Err(super::ToolFailure::new(
                                1,
                                "stdin attachment limit exceeded",
                            ));
                        }
                        Some(bytes)
                    } else {
                        None
                    };
                    let outcome = current.runtime.invoker.invoke_blocking(ToolRequest {
                        name: definition.name.clone(),
                        path: command.path.clone(),
                        input,
                        stdin,
                    });
                    let code = outcome
                        .as_ref()
                        .map_or_else(|e| e.exit_code, |o| o.exit_code);
                    crate::logging::Record::new("tool-invoke")
                        .field("tool", &definition.name)
                        .field("path", command.path.join("/"))
                        .field("exit", code.to_string())
                        .emit(crate::logging::LogFile::Rpc);
                    outcome
                }
            }
        })();
        let code = match result {
            Ok(output) => {
                context.stdout().write_all(&output.stdout)?;
                context.stderr().write_all(&output.stderr)?;
                output.exit_code
            }
            Err(failure) => {
                writeln!(
                    context.stderr(),
                    "{}: {}",
                    context.command_name,
                    failure.message
                )?;
                failure.exit_code
            }
        };
        Ok(ExecutionResult::new(code))
    }
}
