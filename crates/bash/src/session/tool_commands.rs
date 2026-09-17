use super::{env, Arc, Pending, PendingKind, PendingPrompt, Session};
use crate::agent_tools::ToolRuntime;

/// Serializable core prompt continuation. Plug-in continuations belong to the embedder.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ShellContinuation {
    /// Prompt displayed to the caller.
    pub question: String,
    /// Permitted answers, when constrained.
    pub choices: Option<Vec<String>>,
    /// Whether the response must be redacted.
    pub secret: bool,
    /// Original gated command, or a plain user prompt.
    pub command: Option<String>,
    /// Whether approval also grants elevated execution.
    pub sudo_grant: bool,
    /// Input retained for a deferred pipeline.
    pub rerun_stdin: Option<String>,
}

impl Session {
    /// Export a pending core prompt for a stateless shell caller.
    #[must_use]
    pub fn continuation(&self) -> Option<ShellContinuation> {
        let pending = self.pending.as_ref()?;
        let (command, sudo_grant, rerun_stdin) = match &pending.kind {
            PendingKind::UserPrompt => (None, false, None),
            PendingKind::AuthConfirm {
                command,
                sudo_grant,
                rerun_stdin,
            } => (Some(command.clone()), *sudo_grant, rerun_stdin.clone()),
            PendingKind::Plugin(_) => return None,
        };
        Some(ShellContinuation {
            question: pending.prompt.question.clone(),
            choices: pending.prompt.choices.clone(),
            secret: pending.prompt.secret,
            command,
            sudo_grant,
            rerun_stdin,
        })
    }
    /// Restore an outstanding core prompt after shell state has been restored.
    pub fn restore_continuation(&mut self, state: ShellContinuation) {
        let kind = state.command.map_or(PendingKind::UserPrompt, |command| {
            PendingKind::AuthConfirm {
                command,
                sudo_grant: state.sudo_grant,
                rerun_stdin: state.rerun_stdin,
            }
        });
        self.pending = Some(Pending {
            prompt: PendingPrompt {
                question: state.question,
                choices: state.choices,
                secret: state.secret,
            },
            pid: None,
            kind,
        });
    }
    /// Whether this session has the human's blanket confirmation grant.
    #[must_use]
    pub fn confirmation_granted(&self) -> bool {
        self.authz.allow_all
    }
    /// Restore the caller-carried confirmation grant.
    pub fn restore_confirmation_grant(&mut self, granted: bool) {
        self.authz.allow_all = granted;
    }
    /// Attach the owner's tool commands independently of the shell plug-in.
    pub fn set_tools(&mut self, tools: ToolRuntime) {
        if let Some(old) = self.tools.take() {
            for name in old
                .definitions
                .keys()
                .filter(|n| !old.shadowed.contains(*n))
            {
                if let Some(builtin) = self.shell.builtin_mut(name) {
                    builtin.disabled = true;
                }
            }
        }
        self.tools = Some(Arc::new(tools));
        self.refresh_tools();
    }

    pub(super) fn refresh_tools(&mut self) {
        let Some(old) = self.tools.take() else { return };
        let mut tools = ToolRuntime {
            definitions: old.definitions.clone(),
            shadowed: std::collections::BTreeSet::default(),
            invoker: old.invoker.clone(),
        };
        let mut registry = crate::registry::build();
        if let Some(plugin) = &self.plugin {
            for manifest in plugin.manifests() {
                registry.insert(manifest);
            }
        }
        for (name, definition) in &tools.definitions {
            let collision = registry.contains(name)
                || self.shell.builtin_mut(name).is_some_and(|b| {
                    !b.disabled
                        && (old.shadowed.contains(name) || self.registry.get(name).is_none())
                });
            if collision {
                tools.shadowed.insert(name.clone());
                continue;
            }
            self.shell
                .register_builtin(name.clone(), crate::agent_tools::registration());
            registry.insert(
                crate::manifest::Manifest::builtin(name, &definition.nodes[0].doc)
                    .with_policy(crate::manifest::AuthorizationPolicy::Confirm)
                    .with_help(definition.help(0)),
            );
        }
        self.registry = Arc::new(registry);
        let mut dirs = self
            .plugin
            .as_ref()
            .map_or_else(Vec::new, |p| p.path_dirs());
        dirs.push("/usr/lib/tools/bin".into());
        let _ = self.shell.env_mut().set_global(
            "PATH",
            brush_core::variables::ShellVariable::new(env::effective_path(&dirs)),
        );
        // Files are overwritten deterministically; there is no append during replay.
        #[cfg(target_arch = "wasm32")]
        {
            let _ = std::fs::create_dir_all("/usr/lib/tools/bin");
            for (name, definition) in &tools.definitions {
                if !tools.shadowed.contains(name) {
                    let _ =
                        std::fs::write(format!("/usr/lib/tools/bin/{name}"), definition.help(0));
                }
            }
        }
        self.tools = Some(Arc::new(tools));
        self.capabilities = None;
    }

    /// Capture state without executing through the pending-prompt gate.
    pub async fn capture_shell_state(&mut self) -> String {
        let names = self
            .shell
            .env()
            .iter()
            .filter(|(name, _)| !self.secret_env.contains_key(*name))
            .map(|(name, _)| format!("'{}'", name.replace('\'', "'\\'\''")))
            .collect::<Vec<_>>()
            .join(" ");
        let captured = self.execute(&format!("builtin declare -p -- {names}; builtin declare -f; builtin alias -p; builtin set +o; builtin shopt -p")).await;
        String::from_utf8_lossy(&captured.stdout).into_owned()
    }

    /// Restore generated declarations in a fresh session, without treating function bodies as calls.
    ///
    /// # Errors
    /// Rejects execution, redirections, and expansions in the caller-carried declarations.
    pub async fn restore_shell_state(
        &mut self,
        script: &str,
        cwd: &str,
        status: u8,
    ) -> Result<(), String> {
        use brush_parser::ast::{Command, CommandPrefixOrSuffixItem, SeparatorOperator};
        if self.stateless {
            super::stateless::validate(script)?;
        }
        let options = brush_parser::ParserOptions::default();
        let tokens = brush_parser::tokenize_str(script).map_err(|e| e.to_string())?;
        let program = brush_parser::parse_tokens(&tokens, &options).map_err(|e| e.to_string())?;
        let mut declarations = Vec::new();
        let mut functions = Vec::new();
        for list in program.complete_commands {
            for item in list.0 {
                let pipeline = item.0.first;
                if matches!(item.1, SeparatorOperator::Async)
                    || !item.0.additional.is_empty()
                    || pipeline.bang
                    || pipeline.timed.is_some()
                    || pipeline.seq.len() != 1
                {
                    return Err("state must contain declarations, not command execution".into());
                }
                match &pipeline.seq[0] {
                    Command::Function(function) => functions.push(function.to_string()),
                    Command::Simple(simple) => {
                        if simple.prefix.is_some() {
                            return Err("state declaration prefixes are unsupported".into());
                        }
                        let text = simple.to_string();
                        let words = crate::agent_tools::words(&text);
                        let builtin = words.first().is_some_and(|w| w == "builtin");
                        let name = words
                            .get(usize::from(builtin))
                            .ok_or("missing state declaration")?;
                        if !matches!(name.as_str(), "declare" | "alias" | "set" | "shopt") {
                            return Err(format!("state cannot execute {name}"));
                        }
                        let mut immutable = false;
                        if let Some(suffix) = &simple.suffix {
                            for argument in &suffix.0 {
                                let word = match argument {
                                    CommandPrefixOrSuffixItem::Word(word) => word,
                                    CommandPrefixOrSuffixItem::AssignmentWord(assignment, word) => {
                                        let name = assignment.name.to_string();
                                        immutable |= self.shell.env().get(&name).is_some_and(|(_, variable)| variable.is_readonly());
                                        word
                                    }
                                    _ => return Err("state cannot contain redirections or process substitutions".into()),
                                };
                                if !literal_word(&word.value, &options)? {
                                    return Err("state cannot contain value expansions".into());
                                }
                            }
                        }
                        if !immutable {
                            declarations.push(if builtin {
                                text
                            } else {
                                format!("builtin {text}")
                            });
                        }
                    }
                    _ => {
                        return Err(
                            "state must contain declarations and function definitions".into()
                        )
                    }
                }
            }
        }
        declarations.extend(functions);
        let tools = self.tools.take();
        let _scope = crate::agent_tools::suspend();
        let _ = self.execute(&declarations.join("\n")).await;
        self.tools = tools;
        self.shell.set_working_dir(cwd).map_err(|e| e.to_string())?;
        self.shell.set_last_exit_status(status);
        Ok(())
    }
}

fn literal_word(text: &str, options: &brush_parser::ParserOptions) -> Result<bool, String> {
    use brush_parser::word::{WordPiece, WordPieceWithSource};
    fn literal(parts: &[WordPieceWithSource]) -> bool {
        parts.iter().all(|part| match &part.piece {
            WordPiece::Text(_)
            | WordPiece::SingleQuotedText(_)
            | WordPiece::AnsiCQuotedText(_)
            | WordPiece::EscapeSequence(_) => true,
            WordPiece::DoubleQuotedSequence(parts) => literal(parts),
            _ => false,
        })
    }
    brush_parser::word::parse(text, options)
        .map(|parts| literal(&parts))
        .map_err(|e| e.to_string())
}
