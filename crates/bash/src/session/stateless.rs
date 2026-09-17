//! The finite-invocation profile used by bash-tool. Validate code before Brush can park a job,
//! including expanded code introduced by shell builtins. Other shell sessions keep job support.
use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::sync::LazyLock;

use brush_core::builtins::{BoxFuture, Registration};
use brush_core::extensions::DefaultShellExtensions;
use brush_core::{CommandArg, ExecutionContext, ExecutionExitCode, ExecutionResult};
use brush_parser::ast::{Command, CompoundCommand, CompoundList, SeparatorOperator};
use brush_parser::word::{Parameter, ParameterExpr, ParameterTransformOp, WordPiece};

impl super::Session {
    /// Require every shell operation to finish within its invocation. Background jobs, history
    /// execution, dynamic prompt expansion, and sourcing nonregular files are refused with exit 2.
    pub fn enable_stateless_mode(&mut self) {
        self.stateless = true;
        self.shell.options_mut().expand_prompt_strings = false;
        for name in [
            "eval", "source", ".", "alias", "trap", "bg", "fg", "wait", "fc", "shopt",
        ] {
            if let Some(registration) = self.shell.builtin_mut(name) {
                registration.execute_func = execute;
            }
        }
    }
}

static ORIGINALS: LazyLock<HashMap<String, Registration<DefaultShellExtensions>>> =
    LazyLock::new(|| brush_builtins::default_builtins(brush_builtins::BuiltinSet::BashMode));

fn execute(
    context: ExecutionContext<'_>,
    args: Vec<CommandArg>,
) -> BoxFuture<'_, Result<ExecutionResult, brush_core::Error>> {
    Box::pin(async move {
        let name = context.command_name.as_str();
        let words: Vec<String> = args.iter().skip(1).map(ToString::to_string).collect();
        let words = words.strip_prefix(&["--".into()]).unwrap_or(&words);
        let checked = match name {
            "bg" | "fg" | "wait" | "fc" => Err(format!(
                "background/history execution ({name}) is unsupported in bash-tool"
            )),
            "eval" => validate(&words.join(" ")),
            "shopt"
                if words.iter().any(|w| w == "promptvars")
                    && words.iter().any(|w| w.starts_with('-') && w.contains('s')) =>
            {
                Err("dynamic prompt expansion is unsupported in bash-tool".into())
            }
            "source" | "." => words
                .first()
                .map_or(Ok(()), |path| validate_source(&context, path)),
            "alias" => words
                .iter()
                .filter_map(|word| word.split_once('=').map(|(_, value)| value))
                .try_for_each(validate),
            "trap" if words.len() >= 2 && words[0] != "-" && !words[0].starts_with('-') => {
                validate(&words[0])
            }
            _ => Ok(()),
        };
        if let Err(error) = checked {
            writeln!(context.stderr(), "bash: {error}")?;
            return Ok(ExecutionExitCode::from(2).into());
        }
        // Preserve Brush's argument handling, source positional parameters, and control flow.
        (ORIGINALS[name].execute_func)(context, args).await
    })
}

fn validate_source(context: &ExecutionContext<'_>, path: &str) -> Result<(), String> {
    if path.starts_with("/dev/fd/")
        || path.starts_with("/proc/self/fd/")
        || matches!(path, "/dev/stdin" | "/dev/stdout" | "/dev/stderr")
    {
        return Err("source requires a regular file path in bash-tool".into());
    }
    let path = context.shell.absolute_path(std::path::Path::new(path));
    if !std::fs::metadata(&path)
        .map_err(|e| format!("cannot source {}: {e}", path.display()))?
        .is_file()
    {
        return Err("source requires a regular file in bash-tool".into());
    }
    let file =
        std::fs::File::open(&path).map_err(|e| format!("cannot source {}: {e}", path.display()))?;
    if !file.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err("source requires a regular file in bash-tool".into());
    }
    let mut bytes = Vec::new();
    file.take(crate::agent_tools::MAX_ATTACHMENT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > crate::agent_tools::MAX_ATTACHMENT_BYTES {
        return Err("sourced script exceeds the bash-tool input limit".into());
    }
    validate(std::str::from_utf8(&bytes).map_err(|_| "sourced script is not UTF-8")?)
}

pub(super) fn validate(script: &str) -> Result<(), String> {
    script_at_depth(script, 0)
}

fn script_at_depth(script: &str, depth: usize) -> Result<(), String> {
    if depth > 64 {
        return Err("shell code is nested too deeply for bash-tool".into());
    }
    let options = brush_parser::ParserOptions::default();
    let tokens = brush_parser::tokenize_str(script).map_err(|e| e.to_string())?;
    let program = brush_parser::parse_tokens(&tokens, &options).map_err(|e| e.to_string())?;
    for list in &program.complete_commands {
        list_has_background(list, depth)?;
    }
    for token in &tokens {
        if let brush_parser::Token::Word(word, _) = token {
            word_at_depth(word, depth)?;
        }
    }
    Ok(())
}

fn list_has_background(list: &CompoundList, depth: usize) -> Result<(), String> {
    if depth > 64 {
        return Err("shell code is nested too deeply for bash-tool".into());
    }
    for item in &list.0 {
        if matches!(item.1, SeparatorOperator::Async) {
            return Err("background work is unsupported in bash-tool".into());
        }
        for (_, pipeline) in &item.0 {
            for command in &pipeline.seq {
                match command {
                    Command::Compound(command, _) => compound_has_background(command, depth + 1)?,
                    Command::Function(function) => {
                        compound_has_background(&function.body.0, depth + 1)?;
                    }
                    _ => (),
                }
            }
        }
    }
    Ok(())
}

fn compound_has_background(command: &CompoundCommand, depth: usize) -> Result<(), String> {
    match command {
        CompoundCommand::Coprocess(_) => {
            Err("background coprocesses are unsupported in bash-tool".into())
        }
        CompoundCommand::BraceGroup(group) => list_has_background(&group.list, depth),
        CompoundCommand::Subshell(group) => list_has_background(&group.list, depth),
        CompoundCommand::ForClause(clause) => list_has_background(&clause.body.list, depth),
        CompoundCommand::ArithmeticForClause(clause) => {
            list_has_background(&clause.body.list, depth)
        }
        CompoundCommand::CaseClause(clause) => clause
            .cases
            .iter()
            .filter_map(|case| case.cmd.as_ref())
            .try_for_each(|list| list_has_background(list, depth)),
        CompoundCommand::IfClause(clause) => {
            list_has_background(&clause.condition, depth)?;
            list_has_background(&clause.then, depth)?;
            for clause in clause.elses.iter().flatten() {
                if let Some(condition) = &clause.condition {
                    list_has_background(condition, depth)?;
                }
                list_has_background(&clause.body, depth)?;
            }
            Ok(())
        }
        CompoundCommand::WhileClause(clause) | CompoundCommand::UntilClause(clause) => {
            list_has_background(&clause.0, depth)?;
            list_has_background(&clause.1.list, depth)
        }
        CompoundCommand::Arithmetic(command) => word_at_depth(&command.expr.value, depth),
    }
}

fn word_at_depth(word: &str, depth: usize) -> Result<(), String> {
    if depth > 64 {
        return Err("shell expansions are nested too deeply for bash-tool".into());
    }
    let parts = brush_parser::word::parse(word, &brush_parser::ParserOptions::default())
        .map_err(|e| e.to_string())?;
    for part in parts {
        piece_at_depth(&part.piece, depth + 1)?;
    }
    Ok(())
}

fn piece_at_depth(piece: &WordPiece, depth: usize) -> Result<(), String> {
    match piece {
        WordPiece::CommandSubstitution(script)
        | WordPiece::BackquotedCommandSubstitution(script) => script_at_depth(script, depth),
        WordPiece::DoubleQuotedSequence(parts) | WordPiece::GettextDoubleQuotedSequence(parts) => {
            parts
                .iter()
                .try_for_each(|part| piece_at_depth(&part.piece, depth))
        }
        WordPiece::ArithmeticExpression(expression) => word_at_depth(&expression.value, depth),
        WordPiece::ParameterExpansion(expression) => parameter_at_depth(expression, depth),
        _ => Ok(()),
    }
}

fn parameter_at_depth(expression: &ParameterExpr, depth: usize) -> Result<(), String> {
    use ParameterExpr as P;
    let (parameter, values): (&Parameter, Vec<&str>) = match expression {
        P::UseDefaultValues {
            parameter,
            default_value,
            ..
        }
        | P::AssignDefaultValues {
            parameter,
            default_value,
            ..
        } => (parameter, default_value.as_deref().into_iter().collect()),
        P::IndicateErrorIfNullOrUnset {
            parameter,
            error_message,
            ..
        } => (parameter, error_message.as_deref().into_iter().collect()),
        P::UseAlternativeValue {
            parameter,
            alternative_value,
            ..
        } => (
            parameter,
            alternative_value.as_deref().into_iter().collect(),
        ),
        P::RemoveSmallestSuffixPattern {
            parameter, pattern, ..
        }
        | P::RemoveLargestSuffixPattern {
            parameter, pattern, ..
        }
        | P::RemoveSmallestPrefixPattern {
            parameter, pattern, ..
        }
        | P::RemoveLargestPrefixPattern {
            parameter, pattern, ..
        }
        | P::UppercaseFirstChar {
            parameter, pattern, ..
        }
        | P::UppercasePattern {
            parameter, pattern, ..
        }
        | P::LowercaseFirstChar {
            parameter, pattern, ..
        }
        | P::LowercasePattern {
            parameter, pattern, ..
        } => (parameter, pattern.as_deref().into_iter().collect()),
        P::ReplaceSubstring {
            parameter,
            pattern,
            replacement,
            ..
        } => (
            parameter,
            std::iter::once(pattern.as_str())
                .chain(replacement.as_deref())
                .collect(),
        ),
        P::Substring {
            parameter,
            offset,
            length,
            ..
        } => (
            parameter,
            std::iter::once(offset.value.as_str())
                .chain(length.as_ref().map(|e| e.value.as_str()))
                .collect(),
        ),
        P::Transform {
            op: ParameterTransformOp::PromptExpand,
            ..
        } => return Err("dynamic prompt expansion is unsupported in bash-tool".into()),
        P::Parameter { parameter, .. }
        | P::ParameterLength { parameter, .. }
        | P::Transform { parameter, .. } => (parameter, vec![]),
        P::VariableNames { .. } | P::MemberKeys { .. } => return Ok(()),
    };
    if let Parameter::NamedWithIndex { index, .. } = parameter {
        word_at_depth(index, depth)?;
    }
    values
        .into_iter()
        .try_for_each(|word| word_at_depth(word, depth))
}
