//! The plug-in contract, exercised with a small fake plug-in on a real `Session`.

use std::any::Any;

use super::*;
use crate::builtins::promptuser::Resolution;
use crate::plugin::{Plugin, PluginPending, Route};

/// Routes `plug <line>` to itself and runs `<line>` by re-entering dispatch. Counts its runs.
#[derive(Default)]
struct EchoPlugin {
    runs: u32,
    cancelled: Vec<u32>,
}

#[async_trait::async_trait(?Send)]
impl Plugin for EchoPlugin {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
    fn classify_command(&self, line: &str) -> Option<Route> {
        line.strip_prefix("plug ")
            .map(|rest| Route(Box::new(rest.to_string())))
    }
    async fn run(
        &mut self,
        route: Route,
        _line: &str,
        _pid: Option<u32>,
        blanket_authorized: bool,
        ctx: &mut SessionCtx<'_>,
    ) -> LineResult {
        self.runs += 1;
        let Ok(inner) = route.0.downcast::<String>() else {
            return LineResult::stderr("echo-plugin: foreign route\n");
        };
        ctx.run_command(self, &inner, None, blanket_authorized)
            .await
    }
    async fn resume(
        &mut self,
        pending: PluginPending,
        _resolution: Resolution,
        _pid: Option<u32>,
        _ctx: &mut SessionCtx<'_>,
    ) -> LineResult {
        let tag = pending.0.downcast::<&str>().map_or("?", |b| *b);
        LineResult::continue_with_stdout(format!("resumed {tag}\n").into_bytes())
    }
    fn cancel(&mut self, pid: u32, _ctx: &mut SessionCtx<'_>) -> Option<String> {
        (pid == 4242).then(|| {
            self.cancelled.push(pid);
            "[4242] cancelled by plugin\n".to_string()
        })
    }
}

#[test]
fn a_plugin_route_runs_and_re_enters_dispatch_through_itself() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session.set_plugin(Box::new(EchoPlugin::default()));
        let result = session.eval_line("plug plug echo nested").await;
        assert_eq!(result.stdout, b"nested\n");
        assert_eq!(session.plugin_ref::<EchoPlugin>().map(|p| p.runs), Some(2));
    });
}

#[test]
fn kill_asks_the_plugin_first() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session.set_plugin(Box::new(EchoPlugin::default()));
        let result = session.eval_line("kill 4242").await;
        assert_eq!(result.stdout, b"[4242] cancelled by plugin\n");
        assert_eq!(
            session
                .plugin_ref::<EchoPlugin>()
                .map(|p| p.cancelled.clone()),
            Some(vec![4242])
        );
    });
}

#[test]
fn a_plugin_pause_is_resumed_by_answer_prompt() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session.set_plugin(Box::new(EchoPlugin::default()));
        let prompt = crate::builtins::promptuser::PendingPrompt {
            question: "go?".to_string(),
            choices: None,
            secret: false,
        };
        let surfaced = SessionCtx::new(&mut session).surface_pending(
            prompt,
            None,
            PluginPending(Box::new("tag-1")),
        );
        assert!(surfaced.pending_prompt.is_some());
        let resumed = session.answer_prompt(Some("yes".to_string())).await;
        assert_eq!(resumed.stdout, b"resumed tag-1\n");
    });
}

#[test]
fn without_a_plugin_family_commands_are_not_found() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session.clear_plugin_for_test();
        // `sudo` so the `ask` manifest's Confirm policy (still in the core registry until Task 8)
        // does not pause the line before it reaches Brush.
        //
        // Exit 1, not 127: with no plug-in the core `ask` stub builtin (`builtins::interceptstub`)
        // still answers the line. Task 8 moves that stub into the plug-in and changes this to 127.
        let result = session.eval_line("sudo ask hello").await;
        assert_eq!(
            result.exit_code,
            1,
            "stderr: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    });
}
