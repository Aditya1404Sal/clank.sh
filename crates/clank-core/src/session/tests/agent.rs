//! Golem agents as commands: invoke, `--trigger`, `--schedule`, `kill`, and the honest
//! stubs for the verbs no host function backs.
//!
//! Fixtures live in the parent module ([`super`]).

use super::*;

/// End-to-end: `grease install golem:<name>` registers a `/usr/lib/agents/bin/<name>` command;
/// running `<agent> --<ctor> v <method> -- --<arg> v` parses the invocation and dispatches it
/// through the injected invoker (await mode), printing the result. Missing method → exit 2; no
/// invoker → honest "needs a cluster".
#[test]
#[allow(clippy::too_many_lines)]
fn grease_install_then_invoke_a_golem_agent() {
    on_rt(async {
        let _dirs = set_grease_dirs();
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(None));
        session.set_agent_invoker(Box::new(FakeAgentInvoker {
            reply: "added sku abc123".into(),
            seen: seen.clone(),
        }));

        let pkg = serde_json::json!({
            "kind": "agent", "name": "shopping-cart",
            "description": "a shopping cart", "agent-type": "ShoppingCart",
            "constructor-params": ["userid"],
            "methods": [{"name": "add-item", "description": "add an item", "params": ["sku"]}]
        });
        session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![(
            "/packages/",
            grease_json(pkg),
        )])));
        session
            .run_line("grease registry add https://reg.example")
            .await;

        let inst = session.eval_line("sudo grease install shopping-cart").await;
        assert_eq!(
            inst.exit_code,
            0,
            "install stderr: {}",
            String::from_utf8_lossy(&inst.stderr)
        );
        assert!(String::from_utf8(inst.stdout).unwrap().contains("[agent]"));
        assert!(session.grease.is_agent("shopping-cart"));
        // The agent bin stub landed in the agents bin dir.
        assert!(crate::grease::config::agent_bin_dir()
            .join("shopping-cart")
            .exists());

        // `--help` describes the type + methods (no invocation).
        let help = session.eval_line("shopping-cart --help").await;
        assert_eq!(help.exit_code, 0);
        let help_s = String::from_utf8(help.stdout).unwrap();
        assert!(
            help_s.contains("ShoppingCart") && help_s.contains("add-item"),
            "help: {help_s}"
        );

        // `<agent> help` — the bare reserved word (README:840) prints the SAME generated help as
        // `--help`, never treated as a method name (it used to error "unknown method 'help'").
        let bare_help = session.eval_line("shopping-cart help").await;
        assert_eq!(
            bare_help.exit_code,
            0,
            "bare help stderr: {}",
            String::from_utf8_lossy(&bare_help.stderr)
        );
        assert_eq!(
            String::from_utf8(bare_help.stdout).unwrap(),
            help_s,
            "`help` must match `--help`"
        );
        assert!(
            seen.lock().unwrap().is_none(),
            "`help` must not invoke the agent"
        );

        // `sudo <agent> --help` must print the SAME help: sudo only pre-authorizes. It used to look up
        // a package named "sudo", find none, and fall through to the agent parser → exit 2 "unknown
        // flag --help before the method". This is the form that matters most: `ask`'s per-command
        // authorization re-runs an approved command WITH the sudo grant, so a model asking for
        // `<agent> --help` only ever saw the failure — and cannot tell it from a bad flag.
        let sudo_help = session.eval_line("sudo shopping-cart --help").await;
        assert_eq!(
            sudo_help.exit_code,
            0,
            "sudo --help stderr: {}",
            String::from_utf8_lossy(&sudo_help.stderr)
        );
        assert_eq!(
            String::from_utf8(sudo_help.stdout).unwrap(),
            help_s,
            "sudo must not change --help"
        );
        assert!(
            seen.lock().unwrap().is_none(),
            "--help must not invoke the agent"
        );

        // An unknown method → exit 2 (no invocation).
        let bad = session
            .eval_line("sudo shopping-cart --userid jd frobnicate")
            .await;
        assert_eq!(bad.exit_code, 2);
        assert!(String::from_utf8(bad.stderr)
            .unwrap()
            .contains("unknown method"));
        assert!(
            seen.lock().unwrap().is_none(),
            "no invocation on unknown method"
        );

        // Invoke it (sudo pre-authorizes the Confirm) → the invoker sees the parsed invocation.
        let run = session
            .eval_line("sudo shopping-cart --userid jd add-item -- --sku abc123")
            .await;
        assert_eq!(
            run.exit_code,
            0,
            "run stderr: {}",
            String::from_utf8_lossy(&run.stderr)
        );
        assert_eq!(
            String::from_utf8(run.stdout).unwrap().trim_end(),
            "added sku abc123"
        );
        let inv = seen.lock().unwrap().clone().unwrap();
        assert_eq!(inv.agent_type, "ShoppingCart");
        assert_eq!(
            inv.constructor,
            vec![("userid".to_string(), "jd".to_string())]
        );
        assert_eq!(inv.method, "add-item");
        assert_eq!(inv.args, vec![("sku".to_string(), "abc123".to_string())]);

        // A bare (non-sudo) agent run confirms (remote invocation is a Confirm capability).
        let confirm = session
            .eval_line("shopping-cart --userid jd add-item -- --sku x")
            .await;
        assert!(
            confirm.pending_prompt.is_some(),
            "agent run should confirm without sudo"
        );
        session.answer_prompt(Some("no".into())).await;

        // Remove deregisters + deletes the stub.
        let rm = session.eval_line("sudo grease remove shopping-cart").await;
        assert_eq!(rm.exit_code, 0);
        assert!(!session.grease.is_agent("shopping-cart"));
        assert!(!crate::grease::config::agent_bin_dir()
            .join("shopping-cart")
            .exists());
    });
}

/// Without an injected invoker (the native default), an installed agent command reports an honest
/// "needs a cluster" error (exit 4) rather than crashing.
#[test]
fn agent_invocation_without_a_cluster_errors_honestly() {
    on_rt(async {
        let _dirs = set_grease_dirs();
        let mut session = Session::new().await.unwrap(); // no set_agent_invoker
        let pkg = serde_json::json!({
            "kind": "agent", "name": "counter", "description": "c", "agent-type": "Counter",
            "constructor-params": ["id"],
            "methods": [{"name": "increment", "params": []}]
        });
        session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![(
            "/packages/",
            grease_json(pkg),
        )])));
        session
            .run_line("grease registry add https://reg.example")
            .await;
        session.eval_line("sudo grease install counter").await;

        let run = session.eval_line("sudo counter --id x increment").await;
        assert_eq!(run.exit_code, 4);
        assert!(String::from_utf8(run.stderr)
            .unwrap()
            .contains("requires a configured cluster"));
    });
}

/// `--trigger` invokes in fire-and-forget mode: the invoker sees Trigger, a PID row is spawned, and
/// `kill <pid>` clears the tracking (the fire-and-forget can't be cancelled remotely).
#[test]
fn agent_trigger_mode_and_kill() {
    on_rt(async {
        let _dirs = set_grease_dirs();
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(None));
        session.set_agent_invoker(Box::new(FakeAgentInvoker {
            reply: String::new(),
            seen: seen.clone(),
        }));
        install_shopping_cart(&mut session).await;

        let run = session
            .eval_line("sudo shopping-cart --userid jd --trigger add-item -- --sku abc")
            .await;
        assert_eq!(
            run.exit_code,
            0,
            "trigger stderr: {}",
            String::from_utf8_lossy(&run.stderr)
        );
        let out = String::from_utf8(run.stdout).unwrap();
        assert!(out.contains("triggered"), "reports triggered: {out}");
        let inv = seen.lock().unwrap().clone().unwrap();
        assert_eq!(inv.mode, crate::golem::agent::InvokeMode::Trigger);
        assert_eq!(inv.args, vec![("sku".to_string(), "abc".to_string())]);

        // The trigger spawned a PID row; `kill <pid>` clears it. Extract the pid from "[<pid>] …".
        let pid: u32 = out
            .trim_start_matches('[')
            .split(']')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let kill = session.eval_line(&format!("kill {pid}")).await;
        assert_eq!(kill.exit_code, 0);
        assert!(
            String::from_utf8(kill.stdout)
                .unwrap()
                .contains("cannot cancel"),
            "fire-and-forget"
        );
    });
}

/// Fire-and-forget tracking is BOUNDED: a `--trigger`/`--schedule` invocation has no
/// remote-completion signal, so its row would linger forever. Firing many past the cap evicts the
/// oldest (presumed done), so `pending_invocations` never grows without limit.
#[test]
fn trigger_invocation_tracking_is_bounded() {
    on_rt(async {
        let _dirs = set_grease_dirs();
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(None));
        session.set_agent_invoker(Box::new(FakeAgentInvoker {
            reply: String::new(),
            seen: seen.clone(),
        }));
        install_shopping_cart(&mut session).await;

        // Fire well past the cap (MAX_PENDING_INVOCATIONS = 64).
        for i in 0..80 {
            let r = session
                .eval_line(&format!(
                    "sudo shopping-cart --userid jd --trigger add-item -- --sku s{i}"
                ))
                .await;
            assert_eq!(
                r.exit_code,
                0,
                "trigger {i}: {}",
                String::from_utf8_lossy(&r.stderr)
            );
        }
        assert!(
            session.pending_invocations.len() <= 64,
            "fire-and-forget tracking must stay bounded, got {}",
            session.pending_invocations.len()
        );
    });
}

/// `--schedule` reaches Schedule mode with a cancel token; `kill` reports it cancelled.
#[test]
fn agent_schedule_mode_and_kill_cancels() {
    on_rt(async {
        let _dirs = set_grease_dirs();
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(None));
        session.set_agent_invoker(Box::new(FakeAgentInvoker {
            reply: String::new(),
            seen: seen.clone(),
        }));
        install_shopping_cart(&mut session).await;

        let run = session
            .eval_line("sudo shopping-cart --userid jd --schedule 2026-06-01T09:00:00Z add-item -- --sku x")
            .await;
        assert_eq!(
            run.exit_code,
            0,
            "schedule stderr: {}",
            String::from_utf8_lossy(&run.stderr)
        );
        let out = String::from_utf8(run.stdout).unwrap();
        assert!(
            out.contains("scheduled for 2026-06-01"),
            "reports schedule: {out}"
        );
        assert_eq!(
            seen.lock().unwrap().clone().unwrap().mode,
            crate::golem::agent::InvokeMode::Schedule("2026-06-01T09:00:00Z".to_string())
        );
        let pid: u32 = out
            .trim_start_matches('[')
            .split(']')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let kill = session.eval_line(&format!("kill {pid}")).await;
        assert!(
            String::from_utf8(kill.stdout)
                .unwrap()
                .contains("cancelled"),
            "scheduled cancels"
        );
    });
}

/// The honest-stubbed features: `--revision`, reserved `stream`/`repl`, and the ephemeral gate.
#[test]
fn agent_honest_stubs() {
    on_rt(async {
        let _dirs = set_grease_dirs();
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(None));
        session.set_agent_invoker(Box::new(FakeAgentInvoker {
            reply: "ok".into(),
            seen,
        }));
        install_shopping_cart(&mut session).await;

        // --revision → honest exit 2 (no SDK slot).
        let rev = session
            .eval_line("sudo shopping-cart --userid jd --revision 3 add-item -- --sku x")
            .await;
        assert_eq!(rev.exit_code, 2);
        assert!(String::from_utf8(rev.stderr)
            .unwrap()
            .contains("--revision targeting is not supported"));

        // stream/repl → honest (interactive/streaming not on the durable agent).
        let stream = session
            .eval_line("sudo shopping-cart --userid jd stream")
            .await;
        assert_eq!(stream.exit_code, 2);
        assert!(String::from_utf8(stream.stderr)
            .unwrap()
            .contains("interactive/streaming"));
    });
}

/// The `golem` command dispatches through the injected cluster; interrupt/resume are honest-stubbed;
/// no cluster → the honest no-cluster error.
#[test]
fn golem_command_dispatch_and_honest_stubs() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();

        // No cluster injected → honest error (but NOT for interrupt/resume, which are honest anyway).
        let no_cluster = session.eval_line("golem agent list").await;
        assert_eq!(no_cluster.exit_code, 4);
        assert!(String::from_utf8(no_cluster.stderr)
            .unwrap()
            .contains("requires a configured Golem cluster"));

        // interrupt/resume are honest-stubbed regardless of cluster.
        let interrupt = session.eval_line("golem agent interrupt 42").await;
        assert_eq!(interrupt.exit_code, 2);
        assert!(String::from_utf8(interrupt.stderr)
            .unwrap()
            .contains("no guest host binding"));

        // With a cluster: list/status/fork/oplog dispatch.
        session.set_golem_cluster(Box::new(FakeGolemCluster));
        // list/oplog/status are Allow (read-only); fork/rollback are Confirm → sudo pre-authorizes.
        assert!(
            String::from_utf8(session.eval_line("golem agent list").await.stdout)
                .unwrap()
                .contains("agent-1")
        );
        assert!(
            String::from_utf8(session.eval_line("sudo golem fork").await.stdout)
                .unwrap()
                .contains("forked")
        );
        assert!(
            String::from_utf8(session.eval_line("golem oplog").await.stdout)
                .unwrap()
                .contains("self oplog")
        );
        let status = session
            .eval_line("golem agent status --type ShoppingCart --userid jd")
            .await;
        assert!(String::from_utf8(status.stdout)
            .unwrap()
            .contains("status for ShoppingCart"));
        // `type golem` resolves (the new intercepted verb).
        assert!(
            String::from_utf8(session.eval_line("type golem").await.stdout)
                .unwrap()
                .contains("golem")
        );
    });
}
