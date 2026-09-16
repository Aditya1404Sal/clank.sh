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

/// The P-state kill of a plug-in-owned pause, driven through the REAL entry point.
///
/// `eval_line` takes the plug-in out of the slot before `eval_line_inner` runs, so by the time the
/// `kills_pending` branch fires, `session.plugin` is `None` — routing the abort through the public
/// `answer_prompt` (which takes from that now-empty slot) would hand `resume` no plug-in and answer
/// "internal error: plug-in pause with no plug-in" instead. `eval_line_inner` therefore calls
/// `answer_prompt_with_plugin` with the plug-in it already holds, and this test is what fails if
/// that ever regresses.
///
/// It is also the one shape the rest of the suite misses: `kill_asks_the_plugin_first` kills a pid
/// with nothing pending (so it never reaches `kills_pending`), the test above calls `answer_prompt`
/// directly (so it never enters `eval_line_inner`), and `prompt::…` drives the real `kill <pid>`
/// line but only against a `PendingKind::UserPrompt`.
#[test]
fn killing_a_paused_row_aborts_the_plugin_through_real_dispatch() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session.set_plugin(Box::new(EchoPlugin::default()));
        let prompt = crate::builtins::promptuser::PendingPrompt {
            question: "go?".to_string(),
            choices: None,
            secret: false,
        };
        // The pause needs a real process row: `kills_pending` only matches a `kill` whose target pid
        // is the PAUSED row's, so the surfaced pending must carry `Some(pid)`.
        let (pid, surfaced) = {
            let mut ctx = SessionCtx::new(&mut session);
            let pid = ctx.proc_spawn_bg(
                crate::runtime::proctable::ProcessKind::Builtin,
                vec!["plug".to_string()],
                crate::runtime::proctable::SHELL_ROOT_PID,
            );
            let surfaced = ctx.surface_pending(prompt, Some(pid), PluginPending(Box::new("tag-2")));
            (pid, surfaced)
        };
        assert!(surfaced.pending_prompt.is_some());

        // `eval_line`, not `answer_prompt`: the abort has to travel the whole dispatch spine.
        let killed = session.eval_line(&format!("kill {pid}")).await;
        assert_eq!(
            killed.stdout,
            b"resumed tag-2\n",
            "the kill must reach the plug-in's `resume`; stderr: {}",
            String::from_utf8_lossy(&killed.stderr)
        );
        assert_eq!(killed.exit_code, 0);
        assert!(killed.pending_prompt.is_none());
        assert!(
            !session.has_pending_prompt(),
            "the pause must be resolved, not left outstanding"
        );
        // It aborted the pause; it did not fall through to `run_kill`'s `cancel` hook.
        assert_eq!(
            session
                .plugin_ref::<EchoPlugin>()
                .map(|p| p.cancelled.clone()),
            Some(Vec::new())
        );
    });
}

#[test]
fn without_a_plugin_family_commands_are_not_found() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session.clear_plugin_for_test();
        // `sudo` is harmless here (nothing is left to authorize) and pins that the line reaches
        // Brush rather than a gate: with no plug-in installed there is no `ask` manifest, no `ask`
        // builtin and no `ask` route, so the bare shell core answers the way it answers any unknown
        // word — command not found, exit 127.
        let result = session.eval_line("sudo ask hello").await;
        assert_eq!(
            result.exit_code,
            127,
            "stderr: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    });
}
