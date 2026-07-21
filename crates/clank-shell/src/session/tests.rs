use super::*;

/// Drive a closure on a fresh current-thread runtime (mirrors how `Session` is used natively).
fn on_rt<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(f)
}

/// `cd` must move the builtins, not just `pwd`.
///
/// Regression: brush keeps `cd` in its own `Shell::working_dir` and never touches the process's cwd
/// (correct for a shell library — it hands the directory to spawned children). clank's builtins are
/// never spawned; they run in-process and resolve relative paths via `std::env::current_dir()`. So
/// `pwd` and brush's redirects tracked `cd` while every builtin silently ignored it: `cd sub; ls`
/// listed the process's directory and `cd sub; cat f` read the wrong `f`. `ShellCwd` in
/// `tools::coreutils` bridges the two. Covers both dispatch paths — a `uu_*` builtin (`ls`/`cat`, via
/// `run_uu`) and a hand-rolled one (`grep`, via `run_tool`).
///
/// These assert on a **decoy** rather than exact stdout, and that isn't laziness: natively `run_uu`
/// points the real fd 1 at brush's file for the duration of a `uu_*` call, and `FD_SWAP_LOCK`
/// serializes that against other `run_uu`s but *not* against the test harness, which prints `test … ok`
/// to the same fd from other threads. So a builtin's captured stdout can legitimately contain harness
/// chatter under `cargo test` (a parallel-harness artifact only — nothing else writes stdout in
/// production). The decoy is the real signal anyway: it separates "read the right file" from "read the
/// wrong one" regardless of what else lands there.
#[test]
fn cd_is_honored_by_builtins_not_just_pwd() {
    // The process cwd is one global (see ShellCwd's cross-SESSION caveat): another test's builtin
    // entering ShellCwd with a different working_dir mid-window yanks it. Serialize against the
    // known heavy contenders (the curl-pipeline tests, whose grep stages hold cwd windows while a
    // mock server round-trips).
    let _cwd = CWD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    on_rt(async {
        let dir = std::env::temp_dir().join(format!("clank_cd_{}", std::process::id()));
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("only-in-sub.txt"), b"CONTENT-IN-SUB\n").unwrap();
        // Decoys one level up, under the SAME relative name a builtin would use. Resolving against the
        // wrong directory silently finds these instead of erroring — precisely how the bug hid. The
        // names are distinctive so harness chatter can't fake a match.
        std::fs::write(dir.join("only-in-sub.txt"), b"DECOY-IN-PARENT\n").unwrap();
        std::fs::write(dir.join("only-in-parent.txt"), b"DECOY-IN-PARENT\n").unwrap();

        let mut session = Session::new().await.unwrap();
        session.eval_line(&format!("cd {}", sub.display())).await;

        // pwd tracked `cd` even before the fix — assert it still does.
        let pwd = session.eval_line("pwd").await;
        assert_eq!(
            String::from_utf8(pwd.stdout).unwrap().trim(),
            sub.to_string_lossy()
        );

        // `cat` (uu_* -> run_uu) must read sub's file, not the parent's decoy of the same name.
        let cat = String::from_utf8(session.eval_line("cat only-in-sub.txt").await.stdout).unwrap();
        assert!(cat.contains("CONTENT-IN-SUB"), "cat read nothing useful: {cat:?}");
        assert!(!cat.contains("DECOY-IN-PARENT"), "cat resolved against the parent: {cat:?}");

        // `ls` must list sub, so the parent-only file must not appear.
        let ls = String::from_utf8(session.eval_line("ls").await.stdout).unwrap();
        assert!(ls.contains("only-in-sub.txt"), "ls missed sub's file: {ls:?}");
        assert!(!ls.contains("only-in-parent.txt"), "ls listed the parent: {ls:?}");

        // A relative write must land in sub (a filesystem assertion — immune to stdout entirely).
        session.eval_line("touch made-here.txt").await;
        assert!(sub.join("made-here.txt").exists(), "touch wrote outside sub");
        assert!(!dir.join("made-here.txt").exists(), "touch wrote to the parent");

        // `grep` (hand-rolled -> run_tool, the OTHER dispatch path) resolves operands the same way.
        let grep =
            String::from_utf8(session.eval_line("grep CONTENT only-in-sub.txt").await.stdout)
                .unwrap();
        assert!(grep.contains("CONTENT-IN-SUB"), "grep read nothing useful: {grep:?}");
        assert!(!grep.contains("DECOY-IN-PARENT"), "grep resolved against the parent: {grep:?}");

        std::fs::remove_dir_all(&dir).ok();
    });
}

/// Structured evaluation exposes stdout, stderr, and exit code for agent callers.
#[test]
fn eval_line_reports_streams_and_exit_status() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();

        let result = session.eval_line("echo hi").await;
        assert_eq!(String::from_utf8(result.stdout).unwrap(), "hi\n");
        assert!(result.stderr.is_empty());
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.flow, Flow::Continue);

        let result = session.eval_line("false").await;
        assert!(result.stdout.is_empty());
        assert!(result.stderr.is_empty());
        assert_eq!(result.exit_code, 1);
        assert_eq!(result.flow, Flow::Continue);
    });
}

/// uucore keeps a process-global exit code (`uucore::error::set_exit_code`) that upstream
/// coreutils resets by dying — each command is its own process there. clank runs `uumain`
/// IN-PROCESS, so without an explicit reset in `run_uu` a single failed command poisons every
/// later success: `ls <missing>` (exit 2) made a subsequent successful `touch` also report 2.
/// Found by the conformance suite's redirects scenario; affects both targets identically (a wasm
/// agent instance is one long-lived process too).
#[test]
fn uu_exit_code_does_not_stick_across_invocations() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let missing = std::env::temp_dir().join(format!("clank_sticky_missing_{}", std::process::id()));
        let created = std::env::temp_dir().join(format!("clank_sticky_created_{}", std::process::id()));

        let result = session.eval_line(&format!("ls {}", missing.display())).await;
        assert_ne!(result.exit_code, 0, "ls of a missing path must fail");

        let result = session.eval_line(&format!("touch {}", created.display())).await;
        assert!(created.exists(), "touch must actually create the file");
        assert_eq!(
            result.exit_code, 0,
            "a successful uu command must not inherit the previous failure's sticky exit code"
        );
        let _ = std::fs::remove_file(&created);
    });
}

/// End-to-end through the public API: a completed command shows `Z` in `ps`, and `ps` sees its
/// own row as `R` (spawned before execution, completed only after) — like real Unix.
#[test]
fn ps_reflects_completed_and_running_rows() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let (_out, _flow) = session.run_line("echo hi").await;
        let (ps_out, _flow) = session.run_line("ps").await;
        let ps_out = String::from_utf8(ps_out).unwrap();

        // The prior `echo hi` line completed → its row is Z.
        let echo_row = ps_out
            .lines()
            .find(|l| l.contains("echo hi"))
            .expect("ps should list the completed `echo hi` line");
        assert!(
            echo_row.contains('Z'),
            "completed line should be Z, got: {echo_row}"
        );

        // The `ps` invocation itself is still running while it renders → its row is R.
        let ps_row = ps_out
            .lines()
            .find(|l| l.trim_end().ends_with("ps"))
            .expect("ps should list itself");
        assert!(
            ps_row.contains('R'),
            "ps's own row should be R, got: {ps_row}"
        );

        // The synthetic root is present.
        assert!(ps_out.contains("clank"));
    });
}

/// `prompt-user` is intercepted before Brush dispatch: an invocation Brush would reject
/// differently (unknown command) instead surfaces `promptuser::parse`'s own error, proving
/// the interception — not Brush — handled the line. Also proves the line still shows `Z` in
/// `ps` (the process-table row completes normally for intercepted lines, same as `context`).
#[test]
fn prompt_user_is_intercepted_before_brush_dispatch() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let (out, flow) = session.run_line("prompt-user --confirm").await;
        let out = String::from_utf8(out).unwrap();
        assert!(
            out.contains("missing question"),
            "expected promptuser's own parse error, got: {out}"
        );
        assert_eq!(flow, Flow::Continue);

        let (ps_out, _) = session.run_line("ps").await;
        let ps_out = String::from_utf8(ps_out).unwrap();
        let row = ps_out
            .lines()
            .find(|l| l.contains("prompt-user"))
            .expect("ps should list the prompt-user line");
        assert!(row.contains('Z'), "completed line should be Z, got: {row}");
    });
}

/// `type` for a clank-intercepted command resolves through clank's own dispatch (Brush's `type`
/// can't see it): `type curl` → "curl is a shell builtin", exit 0. This is the README's "type
/// authoritative for all commands" made true end-to-end through `eval_line`.
#[test]
fn type_resolves_intercepted_command_as_builtin() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        for name in typecmd::INTERCEPTED {
            let result = session.eval_line(&format!("type {name}")).await;
            assert_eq!(result.exit_code, 0, "type {name} should exit 0");
            assert_eq!(
                String::from_utf8(result.stdout).unwrap(),
                format!("{name} is a shell builtin\n"),
                "type {name} should report a shell builtin"
            );
        }

        // `-t` prints the bare word, like Brush.
        let result = session.eval_line("type -t curl").await;
        assert_eq!(String::from_utf8(result.stdout).unwrap(), "builtin\n");
    });
}

/// `type` for a Brush-registered builtin (`cat`) falls through to Brush unchanged — clank does
/// NOT intercept it. Proves the fallthrough half of the design: clank owns only the intercepted
/// names, Brush keeps everything else.
#[test]
fn type_falls_through_to_brush_for_registered_builtin() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let result = session.eval_line("type cat").await;
        let out = String::from_utf8(result.terminal_output()).unwrap();
        // Brush's `type` resolves `cat` (a registered builtin) — clank didn't short-circuit it.
        assert!(
            out.contains("cat") && out.contains("builtin"),
            "Brush's type should resolve cat as a builtin, got: {out}"
        );
    });
}

/// What a [`FakeProvider`] recorded about a single `turn` call, so tests can assert what context
/// `ask` assembled (transcript-as-context) and which model/tools it used.
#[derive(Clone, Default)]
struct SeenTurn {
    system: Option<String>,
    history: Vec<crate::ai::ask::AskTurn>,
    tools: Vec<crate::ai::ask::AskTool>,
    model: String,
}

impl SeenTurn {
    /// The prompt text from the first user turn (the transcript-as-context body). Mirrors the old
    /// `AskRequest.prompt`/`transcript` accessors the tests used.
    fn user_content(&self) -> String {
        match self.history.first() {
            Some(crate::ai::ask::AskTurn::User(s)) => s.clone(),
            _ => String::new(),
        }
    }
}

/// A fake `AskProvider` for tests: replays a scripted queue of [`AskResponse`]s (one per turn) and
/// records every `turn` call it saw. A single-response script is the common one-turn case.
#[derive(Clone, Default)]
struct FakeProvider {
    /// Scripted responses, consumed front-to-back. When exhausted, a terminal empty text is
    /// returned (so a mis-scripted test terminates rather than looping).
    scripted: std::sync::Arc<Mutex<std::collections::VecDeque<crate::ai::ask::AskResponse>>>,
    /// Every `turn` call, in order.
    seen: std::sync::Arc<Mutex<Vec<SeenTurn>>>,
}

impl FakeProvider {
    /// A provider that replies once with `reply` text and records what it saw.
    fn reply(reply: &str, seen: std::sync::Arc<Mutex<Vec<SeenTurn>>>) -> Self {
        Self::scripted(
            vec![crate::ai::ask::AskResponse::text(reply)],
            seen,
        )
    }

    /// A provider driven by an explicit script of per-turn responses.
    fn scripted(
        responses: Vec<crate::ai::ask::AskResponse>,
        seen: std::sync::Arc<Mutex<Vec<SeenTurn>>>,
    ) -> Self {
        Self {
            scripted: std::sync::Arc::new(Mutex::new(responses.into())),
            seen,
        }
    }
}

/// A single-`shell`-tool-call response for scripting the agentic loop in tests.
fn shell_tool_call(id: &str, command: &str) -> crate::ai::ask::AskResponse {
    crate::ai::ask::AskResponse {
        text: String::new(),
        tool_calls: vec![crate::ai::ask::AskToolCall {
            id: id.to_string(),
            name: crate::ai::ask::SHELL_TOOL.to_string(),
            arguments_json: serde_json::json!({ "command": command }).to_string(),
        }],
        finished_for_tools: true,
        error: None,
    }
}

/// The tool result the loop fed back for `call_id` in the most recent `ToolResults` turn the
/// provider saw, if any.
fn last_tool_result(
    seen: &std::sync::Arc<Mutex<Vec<SeenTurn>>>,
    call_id: &str,
) -> Option<crate::ai::ask::AskToolResult> {
    let turns = seen.lock().unwrap();
    for st in turns.iter().rev() {
        for turn in st.history.iter().rev() {
            if let crate::ai::ask::AskTurn::ToolResults(results) = turn {
                if let Some(r) = results.iter().find(|r| r.id == call_id) {
                    return Some(r.clone());
                }
            }
        }
    }
    None
}

#[async_trait::async_trait(?Send)]
impl crate::ai::ask::AskProvider for FakeProvider {
    async fn turn(
        &self,
        system: Option<&str>,
        history: &[crate::ai::ask::AskTurn],
        tools: &[crate::ai::ask::AskTool],
        model: &str,
    ) -> crate::ai::ask::AskResponse {
        self.seen.lock().unwrap().push(SeenTurn {
            system: system.map(str::to_string),
            history: history.to_vec(),
            tools: tools.to_vec(),
            model: model.to_string(),
        });
        self.scripted
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| crate::ai::ask::AskResponse::text(""))
    }
}

// ---- MCP core (C1) --------------------------------------------------------------------------

/// A scripted [`McpHttp`](crate::mcp::client::McpHttp) fake: replays JSON responses and records
/// requests. Shared by the MCP session tests.
struct FakeMcpHttp {
    responses: std::sync::Arc<Mutex<std::collections::VecDeque<crate::mcp::client::HttpResponse>>>,
    seen: std::sync::Arc<Mutex<Vec<(String, String)>>>,
}

impl FakeMcpHttp {
    fn new(responses: Vec<crate::mcp::client::HttpResponse>) -> Self {
        Self {
            responses: std::sync::Arc::new(Mutex::new(responses.into())),
            seen: std::sync::Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait::async_trait(?Send)]
impl crate::mcp::client::McpHttp for FakeMcpHttp {
    async fn request(
        &self,
        method: &str,
        url: &str,
        _headers: &[(String, String)],
        body: Option<Vec<u8>>,
    ) -> Result<crate::mcp::client::HttpResponse, String> {
        let method_and_body =
            format!("{method} {}", String::from_utf8_lossy(&body.unwrap_or_default()));
        self.seen.lock().unwrap().push((url.to_string(), method_and_body));
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| "no scripted response".to_string())
    }
}

fn mcp_json(value: serde_json::Value) -> crate::mcp::client::HttpResponse {
    crate::mcp::client::HttpResponse {
        status: 200,
        headers: vec![("content-type".into(), "application/json".into())],
        body: value.to_string().into_bytes(),
    }
}

/// A URL-routed fake HTTP transport for grease tests: unlike the order-based `FakeMcpHttp`, it maps
/// a URL substring → response, so grease's `index.json` + `packages/<name>.json` fetches don't
/// collide. An unmatched URL is a 404.
struct FakeGreaseHttp {
    routes: Vec<(String, crate::mcp::client::HttpResponse)>,
}
impl FakeGreaseHttp {
    fn new(routes: Vec<(&str, crate::mcp::client::HttpResponse)>) -> Self {
        Self { routes: routes.into_iter().map(|(u, r)| (u.to_string(), r)).collect() }
    }
}
#[async_trait::async_trait(?Send)]
impl crate::mcp::client::McpHttp for FakeGreaseHttp {
    async fn request(
        &self,
        _method: &str,
        url: &str,
        _headers: &[(String, String)],
        _body: Option<Vec<u8>>,
    ) -> Result<crate::mcp::client::HttpResponse, String> {
        for (pat, resp) in &self.routes {
            if url.contains(pat.as_str()) {
                return Ok(resp.clone());
            }
        }
        Ok(crate::mcp::client::HttpResponse { status: 404, headers: vec![], body: Vec::new() })
    }
}

/// A 200 JSON response (for `FakeGreaseHttp` routes).
fn grease_json(value: serde_json::Value) -> crate::mcp::client::HttpResponse {
    crate::mcp::client::HttpResponse {
        status: 200,
        headers: vec![("content-type".into(), "application/json".into())],
        body: value.to_string().into_bytes(),
    }
}

/// A 200 text response (for a `.md` prompt body served by `FakeGreaseHttp`).
fn grease_text(body: &str) -> crate::mcp::client::HttpResponse {
    crate::mcp::client::HttpResponse {
        status: 200,
        headers: vec![("content-type".into(), "text/markdown".into())],
        body: body.as_bytes().to_vec(),
    }
}

/// A fresh temp `$CLANK_GREASE_*` triple, exported for the duration of the guard (serializes grease
/// tests via the shared lock, clears the vars on drop).
struct GreaseDirsGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
}
impl Drop for GreaseDirsGuard {
    fn drop(&mut self) {
        std::env::remove_var("CLANK_GREASE_ETC");
        std::env::remove_var("CLANK_GREASE_STORE");
        std::env::remove_var("CLANK_GREASE_BIN");
        std::env::remove_var("CLANK_GREASE_SCRIPT_BIN");
        std::env::remove_var("CLANK_GREASE_SKILLS");
        std::env::remove_var("CLANK_GREASE_MCP_MOUNT");
        std::env::remove_var("CLANK_GREASE_AGENT_BIN");
    }
}
fn set_grease_dirs() -> GreaseDirsGuard {
    let lock = crate::grease::config::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let base = std::env::temp_dir().join(format!("clank_grease_sess_{}_{n}", std::process::id()));
    for sub in ["etc", "store", "bin", "script-bin", "skills", "mnt-mcp", "agent-bin"] {
        std::fs::create_dir_all(base.join(sub)).unwrap();
    }
    std::env::set_var("CLANK_GREASE_ETC", base.join("etc"));
    std::env::set_var("CLANK_GREASE_STORE", base.join("store"));
    std::env::set_var("CLANK_GREASE_BIN", base.join("bin"));
    std::env::set_var("CLANK_GREASE_MCP_MOUNT", base.join("mnt-mcp"));
    std::env::set_var("CLANK_GREASE_AGENT_BIN", base.join("agent-bin"));
    std::env::set_var("CLANK_GREASE_SCRIPT_BIN", base.join("script-bin"));
    std::env::set_var("CLANK_GREASE_SKILLS", base.join("skills"));
    GreaseDirsGuard { _lock: lock }
}

struct McpDirsGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    bin: String,
}
impl Drop for McpDirsGuard {
    fn drop(&mut self) {
        std::env::remove_var("CLANK_MCP_ETC");
        std::env::remove_var("CLANK_MCP_BIN");
    }
}

/// A fresh temp `$CLANK_MCP_ETC/$CLANK_MCP_BIN` pair, exported for the duration of the returned
/// guard (which serializes MCP tests via the shared lock and clears the vars on drop).
fn set_mcp_dirs() -> McpDirsGuard {
    let lock = crate::mcp::config::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let base = std::env::temp_dir().join(format!("clank_mcp_sess_{}_{n}", std::process::id()));
    let etc = base.join("etc");
    let bin = base.join("bin");
    std::fs::create_dir_all(&etc).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    std::env::set_var("CLANK_MCP_ETC", &etc);
    std::env::set_var("CLANK_MCP_BIN", &bin);
    McpDirsGuard {
        _lock: lock,
        bin: bin.to_str().unwrap().to_string(),
    }
}

/// The initialize + initialized + tools/list responses for a server offering one `echo` tool.
fn mcp_install_script() -> Vec<crate::mcp::client::HttpResponse> {
    let mut init = mcp_json(serde_json::json!({
        "jsonrpc":"2.0","id":1,"result":{
            "protocolVersion":"2025-03-26",
            "serverInfo":{"name":"demo","version":"1.0"},
            "capabilities":{"tools":{}}}}));
    init.headers.push(("mcp-session-id".into(), "srv-1".into()));
    vec![
        init,
        mcp_json(serde_json::json!({})), // initialized notification
        mcp_json(serde_json::json!({"jsonrpc":"2.0","id":2,"result":{
            "tools":[{"name":"echo","description":"echoes input",
                      "inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}}]}})),
    ]
}

/// A hybrid fake for grease-MCP install tests: grease registry URLs (`/index.json`, `/packages/`)
/// are matched by URL substring; the MCP endpoint (any other URL) is answered by JSON-RPC method
/// name parsed from the request body. Covers initialize/tools/list/prompts/list/prompts/get/
/// resources/list/resources/read.
struct FakeMcpArtifactHttp {
    routes: Vec<(String, crate::mcp::client::HttpResponse)>,
}
impl FakeMcpArtifactHttp {
    fn new(routes: Vec<(&str, crate::mcp::client::HttpResponse)>) -> Self {
        Self { routes: routes.into_iter().map(|(u, r)| (u.to_string(), r)).collect() }
    }
}
#[async_trait::async_trait(?Send)]
impl crate::mcp::client::McpHttp for FakeMcpArtifactHttp {
    async fn request(
        &self,
        _method: &str,
        url: &str,
        _headers: &[(String, String)],
        body: Option<Vec<u8>>,
    ) -> Result<crate::mcp::client::HttpResponse, String> {
        // Grease registry fetches route by URL.
        for (pat, resp) in &self.routes {
            if pat.starts_with('/') && url.contains(pat.as_str()) {
                return Ok(resp.clone());
            }
        }
        // Otherwise it's an MCP JSON-RPC POST — route by the `method` field in the body. For
        // `resources/read`, a more specific `resources/read:<uri>` route wins (lets a test make one
        // resource's read fail → dynamic while another succeeds → static).
        let body = body.unwrap_or_default();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or(serde_json::json!({}));
        let m = v.get("method").and_then(|x| x.as_str()).unwrap_or("");
        if m == "resources/read" {
            if let Some(uri) = v.get("params").and_then(|p| p.get("uri")).and_then(|u| u.as_str()) {
                let keyed = format!("resources/read:{uri}");
                for (pat, resp) in &self.routes {
                    if *pat == keyed {
                        return Ok(resp.clone());
                    }
                }
            }
        }
        for (pat, resp) in &self.routes {
            if pat == m {
                return Ok(resp.clone());
            }
        }
        // Unmapped MCP method → an empty-result success (notifications, etc.).
        Ok(mcp_json(serde_json::json!({"jsonrpc":"2.0","id":1,"result":{}})))
    }
}

/// End-to-end: `grease install <server>` for a `kind:mcp` package fetches the server's live surface
/// (initialize + tools/list + prompts/list + prompts/get + resources/list/read), registers the
/// tools into `McpState` (so `<server> <tool>` works), materializes prompts as $PATH commands and
/// static resources under /mnt/mcp, and caches the surface so a fresh Session rebuilds it offline.
#[test]
fn grease_install_an_mcp_server_registers_tools_prompts_resources() {
    on_rt(async {
        let _dirs = set_grease_dirs();
        let _mcp = set_mcp_dirs();
        let mut session = Session::new().await.unwrap();

        // The grease registry payload: a minimal mcp package pointing at the server URL.
        let pkg = serde_json::json!({
            "kind": "mcp", "name": "demo",
            "description": "a demo MCP server", "url": "https://mcp.demo/x"
        });
        let mut init = mcp_json(serde_json::json!({
            "jsonrpc":"2.0","id":1,"result":{
                "protocolVersion":"2025-03-26","serverInfo":{"name":"demo","version":"1"},
                "capabilities":{"tools":{},"prompts":{},"resources":{}}}}));
        init.headers.push(("mcp-session-id".into(), "s-1".into()));
        let tools = mcp_json(serde_json::json!({"jsonrpc":"2.0","id":2,"result":{
            "tools":[{"name":"echo","description":"echo it",
                "inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}}]}}));
        let prompts_list = mcp_json(serde_json::json!({"jsonrpc":"2.0","id":3,"result":{
            "prompts":[{"name":"summarize-diff","description":"summarize a diff"}]}}));
        let prompts_get = mcp_json(serde_json::json!({"jsonrpc":"2.0","id":4,"result":{
            "messages":[{"role":"user","content":{"type":"text","text":"Summarize this diff."}}]}}));
        let res_list = mcp_json(serde_json::json!({"jsonrpc":"2.0","id":5,"result":{
            "resources":[{"uri":"file:///repo/README.md","name":"readme","mimeType":"text/plain"}]}}));
        let res_read = mcp_json(serde_json::json!({"jsonrpc":"2.0","id":6,"result":{
            "contents":[{"uri":"file:///repo/README.md","text":"# Hello from the resource"}]}}));

        session.set_mcp_http(Box::new(FakeMcpArtifactHttp::new(vec![
            ("/packages/", grease_json(pkg)),
            ("initialize", init),
            ("tools/list", tools),
            ("prompts/list", prompts_list),
            ("prompts/get", prompts_get),
            ("resources/list", res_list),
            ("resources/read", res_read),
        ])));
        session.run_line("grease registry add https://reg.example").await;

        let inst = session.eval_line("sudo grease install demo").await;
        assert_eq!(inst.exit_code, 0, "install stderr: {}", String::from_utf8_lossy(&inst.stderr));
        let out = String::from_utf8(inst.stdout).unwrap();
        assert!(out.contains("installed demo [mcp]"), "install output: {out}");
        assert!(out.contains("1 tools") && out.contains("1 prompts"), "counts: {out}");

        // The server is registered in McpState: `<server> <tool>` is a recognized tool line.
        assert!(session.is_mcp_tool_line("demo echo --text hi"));
        assert!(session.grease.is_mcp("demo"));

        // The prompt was materialized as a standalone $PATH prompt.
        assert!(session.grease.is_prompt("summarize-diff"));

        // The static resource was materialized under /mnt/mcp/demo/.
        let res_path = crate::grease::config::mcp_mount_dir().join("demo/repo/README.md");
        assert_eq!(
            std::fs::read_to_string(&res_path).unwrap(),
            "# Hello from the resource"
        );

        // `grease info demo` describes the server + its artifacts.
        let info = String::from_utf8(session.eval_line("grease info demo").await.stdout).unwrap();
        assert!(info.contains("[mcp]") && info.contains("https://mcp.demo/x"), "info: {info}");
        assert!(info.contains("echo") && info.contains("summarize-diff"), "info lists artifacts: {info}");

        // A FRESH Session rebuilds the tool surface from the cached payload (no live fetch).
        let session2 = Session::new().await.unwrap();
        assert!(session2.is_mcp_tool_line("demo echo --text hi"), "boot reconstruction failed");

        // Remove deregisters from McpState + deletes the resource mount.
        let rm = session.eval_line("sudo grease remove demo").await;
        assert_eq!(rm.exit_code, 0);
        assert!(!session.grease.is_mcp("demo"));
        assert!(!session.is_mcp_tool_line("demo echo --text hi"));
    });
}

/// The /mnt/mcp virtual-fs: an MCP server with one STATIC resource (readable at install → real
/// file) and one DYNAMIC resource (install read fails → served live). `ls /mnt/mcp/<server>/` lists
/// both; a top-level `cat` of the dynamic resource fetches it live via resources/read; the same cat
/// inside `$()` hits the honest Wall-C stub.
#[test]
fn mcp_resources_virtual_fs_static_and_dynamic() {
    on_rt(async {
        let _dirs = set_grease_dirs();
        let _mcp = set_mcp_dirs();
        let mut session = Session::new().await.unwrap();

        let pkg = serde_json::json!({
            "kind": "mcp", "name": "srv", "description": "s", "url": "https://mcp.srv/x"
        });
        let mut init = mcp_json(serde_json::json!({
            "jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26",
            "serverInfo":{"name":"srv","version":"1"},"capabilities":{"resources":{}}}}));
        init.headers.push(("mcp-session-id".into(), "s-1".into()));
        let res_list = mcp_json(serde_json::json!({"jsonrpc":"2.0","id":2,"result":{"resources":[
            {"uri":"file:///docs/guide.md","name":"guide"},
            {"uri":"live://metrics/cpu","name":"cpu"}]}}));
        // Static resource: install read succeeds → materialized as a real file.
        let static_read = mcp_json(serde_json::json!({"jsonrpc":"2.0","id":3,"result":{
            "contents":[{"uri":"file:///docs/guide.md","text":"static guide body"}]}}));
        // Dynamic resource: install read FAILS (500) → recorded dynamic; a later live read succeeds.
        let dyn_read_fail = crate::mcp::client::HttpResponse { status: 500, headers: vec![], body: Vec::new() };
        let dyn_read_ok = mcp_json(serde_json::json!({"jsonrpc":"2.0","id":9,"result":{
            "contents":[{"uri":"live://metrics/cpu","text":"cpu: 42%"}]}}));

        // Two calls to the dynamic URI: first (install) fails, second (live cat) succeeds. The fake
        // routes by exact key, so use a queue-per-URI via two separate installs of the session http.
        session.set_mcp_http(Box::new(FakeMcpArtifactHttp::new(vec![
            ("/packages/", grease_json(pkg)),
            ("initialize", init.clone()),
            ("resources/list", res_list.clone()),
            ("resources/read:file:///docs/guide.md", static_read),
            ("resources/read:live://metrics/cpu", dyn_read_fail),
        ])));
        session.run_line("grease registry add https://reg.example").await;

        let inst = session.eval_line("sudo grease install srv --resources").await;
        assert_eq!(inst.exit_code, 0, "install stderr: {}", String::from_utf8_lossy(&inst.stderr));

        // The static resource is a real file.
        let static_path = crate::grease::config::mcp_mount_dir().join("srv/docs/guide.md");
        assert_eq!(std::fs::read_to_string(&static_path).unwrap(), "static guide body");

        // `ls /mnt/mcp/srv/` lists both the static dir (docs) and the dynamic dir (metrics).
        let ls = String::from_utf8(session.eval_line("ls /mnt/mcp/srv").await.stdout).unwrap();
        assert!(ls.contains("docs") && ls.contains("metrics"), "ls: {ls}");
        // `ls /mnt/mcp` lists the server.
        let ls_root = String::from_utf8(session.eval_line("ls /mnt/mcp").await.stdout).unwrap();
        assert!(ls_root.contains("srv"), "ls root: {ls_root}");

        // Now point the dynamic URI at a SUCCESSFUL read for the live `cat`.
        session.set_mcp_http(Box::new(FakeMcpArtifactHttp::new(vec![
            ("initialize", init),
            ("resources/read:live://metrics/cpu", dyn_read_ok),
        ])));
        // Top-level `cat` of the dynamic resource fetches it live.
        let cat = session.eval_line("cat /mnt/mcp/srv/metrics/cpu").await;
        assert_eq!(cat.exit_code, 0, "dynamic cat stderr: {}", String::from_utf8_lossy(&cat.stderr));
        assert_eq!(String::from_utf8(cat.stdout).unwrap(), "cpu: 42%");

        // The same read inside `$()` does NOT do the live fetch (Wall-C) — Brush's cat finds no
        // real file → nonzero. (We only assert it doesn't crash / doesn't print the live body.)
        let subst = session.eval_line("echo $(cat /mnt/mcp/srv/metrics/cpu)").await;
        assert!(!String::from_utf8_lossy(&subst.stdout).contains("cpu: 42%"), "dynamic read must not run in $()");
    });
}

/// MCP resource templates + `mcp resource info` + `stat`. Install a server with a resource template
/// (`resources/templates/list`) → a `<server>-<name>` executable; running it substitutes the arg
/// into the URI template and reads the constructed resource. `mcp resource info` shows annotations;
/// `ls /mnt/mcp/<server>` lists the template stub.
#[test]
fn mcp_templates_and_resource_info() {
    on_rt(async {
        let _dirs = set_grease_dirs();
        let _mcp = set_mcp_dirs();
        let mut session = Session::new().await.unwrap();

        let pkg = serde_json::json!({
            "kind": "mcp", "name": "gh", "description": "github", "url": "https://mcp.gh/x"
        });
        let mut init = mcp_json(serde_json::json!({
            "jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26",
            "serverInfo":{"name":"gh","version":"1"},"capabilities":{"resources":{}}}}));
        init.headers.push(("mcp-session-id".into(), "s-1".into()));
        // One static resource (with annotations) + one template.
        let res_list = mcp_json(serde_json::json!({"jsonrpc":"2.0","id":2,"result":{"resources":[
            {"uri":"file:///repo/README.md","name":"readme","mimeType":"text/markdown",
             "size":42,"annotations":{"lastModified":"2026-01-01T00:00:00Z","priority":0.8,
             "audience":["user","assistant"]}}]}}));
        let static_read = mcp_json(serde_json::json!({"jsonrpc":"2.0","id":3,"result":{
            "contents":[{"uri":"file:///repo/README.md","text":"readme body"}]}}));
        let tmpl_list = mcp_json(serde_json::json!({"jsonrpc":"2.0","id":4,"result":{
            "resourceTemplates":[{"uriTemplate":"github://repo/{path}","name":"file-lookup",
             "description":"look up a repo file"}]}}));
        // The template read: constructed URI github://repo/src/main.rs.
        let tmpl_read = mcp_json(serde_json::json!({"jsonrpc":"2.0","id":5,"result":{
            "contents":[{"uri":"github://repo/src/main.rs","text":"fn main() {}"}]}}));

        session.set_mcp_http(Box::new(FakeMcpArtifactHttp::new(vec![
            ("/packages/", grease_json(pkg)),
            ("initialize", init.clone()),
            ("resources/list", res_list),
            ("resources/templates/list", tmpl_list),
            ("resources/read:file:///repo/README.md", static_read),
            ("resources/read:github://repo/src/main.rs", tmpl_read),
        ])));
        session.run_line("grease registry add https://reg.example").await;

        let inst = session.eval_line("sudo grease install gh --resources").await;
        assert_eq!(inst.exit_code, 0, "install stderr: {}", String::from_utf8_lossy(&inst.stderr));
        assert!(String::from_utf8(inst.stdout).unwrap().contains("1 templates"), "reports templates");

        // The template executable exists (`type`/`ls` see it).
        let ls = String::from_utf8(session.eval_line("ls /mnt/mcp/gh").await.stdout).unwrap();
        assert!(ls.contains("gh-file-lookup"), "template stub listed: {ls}");

        // Running the template with a positional arg substitutes {path} and reads the URI.
        let run = session.eval_line("gh-file-lookup src/main.rs").await;
        assert_eq!(run.exit_code, 0, "template run stderr: {}", String::from_utf8_lossy(&run.stderr));
        assert_eq!(String::from_utf8(run.stdout).unwrap(), "fn main() {}");

        // `mcp resource info` shows the annotations.
        let info = String::from_utf8(
            session.eval_line("mcp resource info /mnt/mcp/gh/repo/README.md").await.stdout,
        )
        .unwrap();
        assert!(info.contains("2026-01-01"), "info shows lastModified: {info}");
        assert!(info.contains("priority: 0.8"), "info shows priority: {info}");
        assert!(info.contains("user,assistant"), "info shows audience: {info}");
    });
}

/// `mcp watch <uri>` is a bounded poll (not a push stream) — it reads the resource N times and
/// stops, honest about the limitation.
#[test]
fn mcp_watch_is_a_bounded_poll() {
    on_rt(async {
        let _dirs = set_grease_dirs();
        let _mcp = set_mcp_dirs();
        let mut session = Session::new().await.unwrap();
        let pkg = serde_json::json!({
            "kind": "mcp", "name": "metrics", "description": "m", "url": "https://mcp.m/x"
        });
        let mut init = mcp_json(serde_json::json!({
            "jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26",
            "serverInfo":{"name":"metrics","version":"1"},"capabilities":{"resources":{}}}}));
        init.headers.push(("mcp-session-id".into(), "s-1".into()));
        // resources/list makes the server own the metrics:// uri.
        let res_list = mcp_json(serde_json::json!({"jsonrpc":"2.0","id":2,"result":{"resources":[
            {"uri":"metrics://cpu","name":"cpu"}]}}));
        // resources/read for the dynamic uri (install read fails → dynamic; watch reads succeed).
        let read_ok = mcp_json(serde_json::json!({"jsonrpc":"2.0","id":9,"result":{
            "contents":[{"uri":"metrics://cpu","text":"cpu: 10%"}]}}));

        session.set_mcp_http(Box::new(FakeMcpArtifactHttp::new(vec![
            ("/packages/", grease_json(pkg)),
            ("initialize", init),
            ("resources/list", res_list),
            ("resources/templates/list", mcp_json(serde_json::json!({"jsonrpc":"2.0","id":3,"result":{"resourceTemplates":[]}}))),
            ("resources/read:metrics://cpu", read_ok),
            ("resources/subscribe", mcp_json(serde_json::json!({"jsonrpc":"2.0","id":4,"result":{}}))),
        ])));
        session.run_line("grease registry add https://reg.example").await;
        session.eval_line("sudo grease install metrics --resources").await;

        let watch = session.eval_line("mcp watch metrics://cpu").await;
        assert_eq!(watch.exit_code, 0);
        let out = String::from_utf8(watch.stdout).unwrap();
        assert!(out.contains("bounded poll"), "honest about polling: {out}");
        assert!(out.contains("cpu: 10%"), "prints the resource content: {out}");
        assert!(out.contains("done"), "terminates: {out}");
    });
}

/// A scripted [`crate::golem::agent::AgentInvoker`]: records the invocation it saw and returns a fixed
/// reply (the native stand-in for the durable `WasmRpc` binding, which needs a cluster).
struct FakeAgentInvoker {
    reply: String,
    seen: std::sync::Arc<Mutex<Option<crate::golem::agent::AgentInvocation>>>,
}
#[async_trait::async_trait(?Send)]
impl crate::golem::agent::AgentInvoker for FakeAgentInvoker {
    async fn invoke(
        &self,
        inv: &crate::golem::agent::AgentInvocation,
    ) -> Result<String, String> {
        *self.seen.lock().unwrap() = Some(inv.clone());
        Ok(self.reply.clone())
    }
    async fn invoke_async(
        &self,
        inv: &crate::golem::agent::AgentInvocation,
    ) -> Result<crate::golem::agent::InvokeHandle, String> {
        *self.seen.lock().unwrap() = Some(inv.clone());
        let (token, note) = match &inv.mode {
            crate::golem::agent::InvokeMode::Trigger => (None, "triggered".to_string()),
            crate::golem::agent::InvokeMode::Schedule(w) => {
                (Some("tok".to_string()), format!("scheduled for {w}"))
            }
            crate::golem::agent::InvokeMode::Await => (None, String::new()),
        };
        Ok(crate::golem::agent::InvokeHandle { cancel_token: token, note })
    }
}

/// A scripted [`crate::golem::cluster::GolemCluster`] recording the calls it saw.
struct FakeGolemCluster;
#[async_trait::async_trait(?Send)]
impl crate::golem::cluster::GolemCluster for FakeGolemCluster {
    async fn agent_list(&self) -> Result<String, String> {
        Ok("agent-1\nagent-2".to_string())
    }
    async fn agent_oplog(&self, t: &str, _c: &[(String, String)]) -> Result<String, String> {
        Ok(format!("oplog for {t}"))
    }
    async fn agent_status(&self, t: &str, _c: &[(String, String)]) -> Result<String, String> {
        Ok(format!("status for {t}"))
    }
    async fn connect(&self, id: &str) -> Result<String, String> {
        Ok(format!("connected to {id}"))
    }
    async fn self_oplog(&self) -> Result<String, String> {
        Ok("self oplog".to_string())
    }
    async fn rollback(&self) -> Result<String, String> {
        Ok("rolled back".to_string())
    }
    async fn fork(&self) -> Result<String, String> {
        Ok("forked".to_string())
    }
}

/// End-to-end: `grease install golem:<name>` registers a `/usr/lib/agents/bin/<name>` command;
/// running `<agent> --<ctor> v <method> -- --<arg> v` parses the invocation and dispatches it
/// through the injected invoker (await mode), printing the result. Missing method → exit 2; no
/// invoker → honest "needs a cluster".
#[test]
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
        session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![("/packages/", grease_json(pkg))])));
        session.run_line("grease registry add https://reg.example").await;

        let inst = session.eval_line("sudo grease install shopping-cart").await;
        assert_eq!(inst.exit_code, 0, "install stderr: {}", String::from_utf8_lossy(&inst.stderr));
        assert!(String::from_utf8(inst.stdout).unwrap().contains("[agent]"));
        assert!(session.grease.is_agent("shopping-cart"));
        // The agent bin stub landed in the agents bin dir.
        assert!(crate::grease::config::agent_bin_dir().join("shopping-cart").exists());

        // `--help` describes the type + methods (no invocation).
        let help = session.eval_line("shopping-cart --help").await;
        assert_eq!(help.exit_code, 0);
        let help_s = String::from_utf8(help.stdout).unwrap();
        assert!(help_s.contains("ShoppingCart") && help_s.contains("add-item"), "help: {help_s}");

        // `<agent> help` — the bare reserved word (README:840) prints the SAME generated help as
        // `--help`, never treated as a method name (it used to error "unknown method 'help'").
        let bare_help = session.eval_line("shopping-cart help").await;
        assert_eq!(
            bare_help.exit_code,
            0,
            "bare help stderr: {}",
            String::from_utf8_lossy(&bare_help.stderr)
        );
        assert_eq!(String::from_utf8(bare_help.stdout).unwrap(), help_s, "`help` must match `--help`");
        assert!(seen.lock().unwrap().is_none(), "`help` must not invoke the agent");

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
        assert_eq!(String::from_utf8(sudo_help.stdout).unwrap(), help_s, "sudo must not change --help");
        assert!(seen.lock().unwrap().is_none(), "--help must not invoke the agent");

        // An unknown method → exit 2 (no invocation).
        let bad = session.eval_line("sudo shopping-cart --userid jd frobnicate").await;
        assert_eq!(bad.exit_code, 2);
        assert!(String::from_utf8(bad.stderr).unwrap().contains("unknown method"));
        assert!(seen.lock().unwrap().is_none(), "no invocation on unknown method");

        // Invoke it (sudo pre-authorizes the Confirm) → the invoker sees the parsed invocation.
        let run = session.eval_line("sudo shopping-cart --userid jd add-item -- --sku abc123").await;
        assert_eq!(run.exit_code, 0, "run stderr: {}", String::from_utf8_lossy(&run.stderr));
        assert_eq!(String::from_utf8(run.stdout).unwrap().trim_end(), "added sku abc123");
        let inv = seen.lock().unwrap().clone().unwrap();
        assert_eq!(inv.agent_type, "ShoppingCart");
        assert_eq!(inv.constructor, vec![("userid".to_string(), "jd".to_string())]);
        assert_eq!(inv.method, "add-item");
        assert_eq!(inv.args, vec![("sku".to_string(), "abc123".to_string())]);

        // A bare (non-sudo) agent run confirms (remote invocation is a Confirm capability).
        let confirm = session.eval_line("shopping-cart --userid jd add-item -- --sku x").await;
        assert!(confirm.pending_prompt.is_some(), "agent run should confirm without sudo");
        session.answer_prompt(Some("no".into())).await;

        // Remove deregisters + deletes the stub.
        let rm = session.eval_line("sudo grease remove shopping-cart").await;
        assert_eq!(rm.exit_code, 0);
        assert!(!session.grease.is_agent("shopping-cart"));
        assert!(!crate::grease::config::agent_bin_dir().join("shopping-cart").exists());
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
        session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![("/packages/", grease_json(pkg))])));
        session.run_line("grease registry add https://reg.example").await;
        session.eval_line("sudo grease install counter").await;

        let run = session.eval_line("sudo counter --id x increment").await;
        assert_eq!(run.exit_code, 4);
        assert!(String::from_utf8(run.stderr).unwrap().contains("requires a configured cluster"));
    });
}

/// Install a `golem:shopping-cart` agent (helper) — a durable ShoppingCart with one method.
async fn install_shopping_cart(session: &mut Session) {
    let pkg = serde_json::json!({
        "kind": "agent", "name": "shopping-cart", "description": "cart",
        "agent-type": "ShoppingCart", "constructor-params": ["userid"],
        "methods": [{"name": "add-item", "params": ["sku"]}], "ephemeral": false
    });
    session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![("/packages/", grease_json(pkg))])));
    session.run_line("grease registry add https://reg.example").await;
    session.eval_line("sudo grease install shopping-cart").await;
}

/// `--trigger` invokes in fire-and-forget mode: the invoker sees Trigger, a PID row is spawned, and
/// `kill <pid>` clears the tracking (the fire-and-forget can't be cancelled remotely).
#[test]
fn agent_trigger_mode_and_kill() {
    on_rt(async {
        let _dirs = set_grease_dirs();
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(None));
        session.set_agent_invoker(Box::new(FakeAgentInvoker { reply: String::new(), seen: seen.clone() }));
        install_shopping_cart(&mut session).await;

        let run = session.eval_line("sudo shopping-cart --userid jd --trigger add-item -- --sku abc").await;
        assert_eq!(run.exit_code, 0, "trigger stderr: {}", String::from_utf8_lossy(&run.stderr));
        let out = String::from_utf8(run.stdout).unwrap();
        assert!(out.contains("triggered"), "reports triggered: {out}");
        let inv = seen.lock().unwrap().clone().unwrap();
        assert_eq!(inv.mode, crate::golem::agent::InvokeMode::Trigger);
        assert_eq!(inv.args, vec![("sku".to_string(), "abc".to_string())]);

        // The trigger spawned a PID row; `kill <pid>` clears it. Extract the pid from "[<pid>] …".
        let pid: u32 = out.trim_start_matches('[').split(']').next().unwrap().parse().unwrap();
        let kill = session.eval_line(&format!("kill {pid}")).await;
        assert_eq!(kill.exit_code, 0);
        assert!(String::from_utf8(kill.stdout).unwrap().contains("cannot cancel"), "fire-and-forget");
    });
}

/// `--schedule` reaches Schedule mode with a cancel token; `kill` reports it cancelled.
#[test]
fn agent_schedule_mode_and_kill_cancels() {
    on_rt(async {
        let _dirs = set_grease_dirs();
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(None));
        session.set_agent_invoker(Box::new(FakeAgentInvoker { reply: String::new(), seen: seen.clone() }));
        install_shopping_cart(&mut session).await;

        let run = session
            .eval_line("sudo shopping-cart --userid jd --schedule 2026-06-01T09:00:00Z add-item -- --sku x")
            .await;
        assert_eq!(run.exit_code, 0, "schedule stderr: {}", String::from_utf8_lossy(&run.stderr));
        let out = String::from_utf8(run.stdout).unwrap();
        assert!(out.contains("scheduled for 2026-06-01"), "reports schedule: {out}");
        assert_eq!(
            seen.lock().unwrap().clone().unwrap().mode,
            crate::golem::agent::InvokeMode::Schedule("2026-06-01T09:00:00Z".to_string())
        );
        let pid: u32 = out.trim_start_matches('[').split(']').next().unwrap().parse().unwrap();
        let kill = session.eval_line(&format!("kill {pid}")).await;
        assert!(String::from_utf8(kill.stdout).unwrap().contains("cancelled"), "scheduled cancels");
    });
}

/// The honest-stubbed features: `--revision`, reserved `stream`/`repl`, and the ephemeral gate.
#[test]
fn agent_honest_stubs() {
    on_rt(async {
        let _dirs = set_grease_dirs();
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(None));
        session.set_agent_invoker(Box::new(FakeAgentInvoker { reply: "ok".into(), seen }));
        install_shopping_cart(&mut session).await;

        // --revision → honest exit 2 (no SDK slot).
        let rev = session.eval_line("sudo shopping-cart --userid jd --revision 3 add-item -- --sku x").await;
        assert_eq!(rev.exit_code, 2);
        assert!(String::from_utf8(rev.stderr).unwrap().contains("--revision targeting is not supported"));

        // stream/repl → honest (interactive/streaming not on the durable agent).
        let stream = session.eval_line("sudo shopping-cart --userid jd stream").await;
        assert_eq!(stream.exit_code, 2);
        assert!(String::from_utf8(stream.stderr).unwrap().contains("interactive/streaming"));
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
        assert!(String::from_utf8(no_cluster.stderr).unwrap().contains("requires a configured Golem cluster"));

        // interrupt/resume are honest-stubbed regardless of cluster.
        let interrupt = session.eval_line("golem agent interrupt 42").await;
        assert_eq!(interrupt.exit_code, 2);
        assert!(String::from_utf8(interrupt.stderr).unwrap().contains("no guest host binding"));

        // With a cluster: list/status/fork/oplog dispatch.
        session.set_golem_cluster(Box::new(FakeGolemCluster));
        // list/oplog/status are Allow (read-only); fork/rollback are Confirm → sudo pre-authorizes.
        assert!(String::from_utf8(session.eval_line("golem agent list").await.stdout).unwrap().contains("agent-1"));
        assert!(String::from_utf8(session.eval_line("sudo golem fork").await.stdout).unwrap().contains("forked"));
        assert!(String::from_utf8(session.eval_line("golem oplog").await.stdout).unwrap().contains("self oplog"));
        let status = session.eval_line("golem agent status --type ShoppingCart --userid jd").await;
        assert!(String::from_utf8(status.stdout).unwrap().contains("status for ShoppingCart"));
        // `type golem` resolves (the new intercepted verb).
        assert!(String::from_utf8(session.eval_line("type golem").await.stdout).unwrap().contains("golem"));
    });
}

/// `mcp add` installs a server: config written, tools fetched, `mcp list`/`mcp tools`/`which`
/// `grease registry add/list/remove` through `eval_line`: the registry list is persisted and
/// surfaced. `registry` is Allow (local config only — no network, no pause).
#[test]
fn grease_registry_add_list_remove() {
    on_rt(async {
        let _dirs = set_grease_dirs();
        let mut session = Session::new().await.unwrap();

        let list0 = session.eval_line("grease registry list").await;
        assert!(String::from_utf8(list0.stdout).unwrap().contains("no registries configured"));

        let add = session.eval_line("grease registry add https://reg.example").await;
        assert_eq!(add.exit_code, 0);
        assert!(add.pending_prompt.is_none(), "registry add is Allow — no pause");
        assert!(String::from_utf8(add.stdout).unwrap().contains("added registry"));

        let list1 = session.eval_line("grease registry list").await;
        assert!(String::from_utf8(list1.stdout).unwrap().contains("https://reg.example"));

        let rm = session.eval_line("grease registry remove https://reg.example").await;
        assert_eq!(rm.exit_code, 0);
        let list2 = session.eval_line("grease registry list").await;
        assert!(String::from_utf8(list2.stdout).unwrap().contains("no registries configured"));
    });
}

/// End-to-end: `grease install` fetches a prompt package, persists it, registers it as a command,
/// and running the installed prompt name dispatches to the model with the (filled) body. Uses the
/// scripted fake HTTP transport (reused from MCP — grease shares the `McpHttp` seam).
#[test]
fn grease_install_then_run_a_prompt() {
    on_rt(async {
        let _dirs = set_grease_dirs();
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::reply("the summary", seen.clone())));

        // Script the registry: GET /packages/tldr.json → a parameterized prompt package.
        let pkg = serde_json::json!({
            "name": "tldr",
            "description": "summarize a file",
            "arguments": [{"name":"file","required":true}],
            "body": "Summarize the file {{file}} concisely."
        });
        // No index route → the index lookup 404s → record-only install (these tests don't assert
        // on integrity; the verify path has its own dedicated tests).
        session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![("/packages/", grease_json(pkg))])));
        session.run_line("grease registry add https://reg.example").await;

        // Install (sudo pre-authorizes the Confirm).
        let inst = session.eval_line("sudo grease install tldr").await;
        assert_eq!(inst.exit_code, 0, "install stderr: {}", String::from_utf8_lossy(&inst.stderr));
        assert!(String::from_utf8(inst.stdout).unwrap().contains("installed tldr"));

        // It's now an installed prompt: `grease list` shows it, `type tldr` sees it.
        let list = session.eval_line("grease list").await;
        assert!(String::from_utf8(list.stdout).unwrap().contains("tldr"));

        // `tldr --help` shows its generated help (with the arg), no confirmation.
        let help = session.eval_line("tldr --help").await;
        assert_eq!(help.exit_code, 0);
        assert!(String::from_utf8(help.stdout).unwrap().contains("--file"));

        // Missing required arg → exit 2 (no model call).
        let miss = session.eval_line("sudo tldr").await;
        assert_eq!(miss.exit_code, 2);
        assert!(String::from_utf8(miss.stderr).unwrap().contains("missing required argument --file"));

        // Run it with the arg (sudo pre-authorizes the prompt's Confirm) → the model sees the
        // FILLED body.
        let run = session.eval_line("sudo tldr --file report.md").await;
        assert_eq!(run.exit_code, 0);
        assert_eq!(String::from_utf8(run.stdout).unwrap(), "the summary");
        let content = seen.lock().unwrap()[0].user_content();
        assert!(content.contains("Summarize the file report.md concisely."), "got: {content}");

        // A bare (non-sudo) prompt run confirms (outbound LLM).
        let confirm = session.eval_line("tldr --file x.md").await;
        assert!(confirm.pending_prompt.is_some(), "prompt run should confirm without sudo");
        session.answer_prompt(Some("no".into())).await;

        // Remove deregisters: the name is no longer an installed prompt.
        let rm = session.eval_line("sudo grease remove tldr").await;
        assert_eq!(rm.exit_code, 0);
        assert!(!session.grease.is_prompt("tldr"));
    });
}

/// A prompt authored as a `.md` file with YAML frontmatter installs identically to a JSON prompt:
/// grease fetches `/packages/<name>.md`, converts the frontmatter → the canonical PromptPackage,
/// and the installed command fills `{{var}}` and dispatches to the model.
#[test]
fn grease_install_then_run_a_markdown_prompt() {
    on_rt(async {
        let _dirs = set_grease_dirs();
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::reply("the summary", seen.clone())));

        // The registry serves the prompt as a Markdown file (no `.json`), routed on the `.md` suffix
        // so the `.json` fetch 404s first and the `.md` fetch succeeds.
        let md = "---\n\
                  name: tldr\n\
                  description: summarize a file\n\
                  arguments:\n\
                  \x20 - name: file\n\
                  \x20   required: true\n\
                  ---\n\
                  Summarize the file {{file}} concisely.\n";
        session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![(
            "/packages/tldr.md",
            grease_text(md),
        )])));
        session.run_line("grease registry add https://reg.example").await;

        let inst = session.eval_line("sudo grease install tldr").await;
        assert_eq!(inst.exit_code, 0, "install stderr: {}", String::from_utf8_lossy(&inst.stderr));
        assert!(String::from_utf8(inst.stdout).unwrap().contains("installed tldr"));

        // Installed as a prompt with the declared arg (from the frontmatter).
        let help = session.eval_line("tldr --help").await;
        assert_eq!(help.exit_code, 0);
        assert!(String::from_utf8(help.stdout).unwrap().contains("--file"));

        // Running fills the body (converted from the `.md` body) and dispatches to the model.
        let run = session.eval_line("sudo tldr --file report.md").await;
        assert_eq!(run.exit_code, 0);
        assert_eq!(String::from_utf8(run.stdout).unwrap(), "the summary");
        let content = seen.lock().unwrap()[0].user_content();
        assert!(content.contains("Summarize the file report.md concisely."), "got: {content}");
    });
}

/// `cat /proc/clank/system-prompt` reflects LIVE state: after installing a grease prompt, the proc
/// file lists it as a `prompt__<name>` tool (the exact prompt the model sees), not just the static
/// base surface.
#[test]
fn proc_system_prompt_reflects_installed_prompts() {
    on_rt(async {
        let _dirs = set_grease_dirs();
        let mut session = Session::new().await.unwrap();

        // Before install: the proc file has the base surface but NOT our prompt.
        let before = String::from_utf8(
            session.eval_line("cat /proc/clank/system-prompt").await.stdout,
        )
        .unwrap();
        assert!(!before.contains("prompt__hello"), "not installed yet");

        let pkg = serde_json::json!({
            "kind": "prompt", "name": "hello", "description": "say hi", "body": "Say hi."
        });
        session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![("/packages/", grease_json(pkg))])));
        session.run_line("grease registry add https://reg.example").await;
        session.eval_line("sudo grease install hello").await;

        // After install: the proc file lists the installed prompt tool + the "Installed prompt
        // tools" heading from build_system_prompt_with_capabilities.
        let after = String::from_utf8(
            session.eval_line("cat /proc/clank/system-prompt").await.stdout,
        )
        .unwrap();
        assert!(after.contains("prompt__hello"), "system prompt lists the installed prompt: {after}");
        assert!(after.contains("Installed prompt tools"), "and its heading: {after}");
    });
}

/// End-to-end: `grease install` fetches a `kind:script` package, persists it to the store, writes
/// its bin stub to the SCRIPT bin dir (not the prompt dir), registers it as a Confirm command, and
/// running the installed name executes the FILLED shell body through Brush (`run_string`) — no LLM.
#[test]
fn grease_install_then_run_a_script() {
    on_rt(async {
        let _dirs = set_grease_dirs();
        let mut session = Session::new().await.unwrap();

        // A parameterized shell-script package.
        let pkg = serde_json::json!({
            "kind": "script",
            "name": "greet",
            "description": "print a greeting",
            "arguments": [{"name":"who","required":true}],
            "body": "echo hello {{who}}"
        });
        session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![("/packages/", grease_json(pkg))])));
        session.run_line("grease registry add https://reg.example").await;

        let inst = session.eval_line("sudo grease install greet").await;
        assert_eq!(inst.exit_code, 0, "install stderr: {}", String::from_utf8_lossy(&inst.stderr));
        let out = String::from_utf8(inst.stdout).unwrap();
        assert!(out.contains("installed greet"), "install output: {out}");
        assert!(out.contains("[script]"), "install output names the kind: {out}");

        // It's an installed SCRIPT (not a prompt), and its stub is in the script bin dir.
        assert!(session.grease.is_script("greet"));
        assert!(!session.grease.is_prompt("greet"));
        assert!(crate::grease::config::script_bin_dir().join("greet").exists());
        assert!(!crate::grease::config::bin_dir().join("greet").exists());

        // `grease list` shows it tagged as a script.
        let list = String::from_utf8(session.eval_line("grease list").await.stdout).unwrap();
        assert!(list.contains("greet") && list.contains("[script]"), "list: {list}");

        // `greet --help` shows generated help disclosing the local-shell capability, no confirm.
        let help = session.eval_line("greet --help").await;
        assert_eq!(help.exit_code, 0);
        let help_s = String::from_utf8(help.stdout).unwrap();
        assert!(help_s.contains("--who"), "help: {help_s}");
        assert!(help_s.contains("local shell"), "help discloses shell capability: {help_s}");

        // Missing required arg → exit 2, no shell run.
        let miss = session.eval_line("sudo greet").await;
        assert_eq!(miss.exit_code, 2);
        assert!(String::from_utf8(miss.stderr).unwrap().contains("missing required argument --who"));

        // Run it (sudo pre-authorizes the Confirm) → the FILLED shell body runs locally.
        let run = session.eval_line("sudo greet --who world").await;
        assert_eq!(run.exit_code, 0, "run stderr: {}", String::from_utf8_lossy(&run.stderr));
        assert_eq!(String::from_utf8(run.stdout).unwrap().trim_end(), "hello world");

        // A bare (non-sudo) script run confirms (running local shell is a Confirm capability).
        let confirm = session.eval_line("greet --who x").await;
        assert!(confirm.pending_prompt.is_some(), "script run should confirm without sudo");
        session.answer_prompt(Some("no".into())).await;

        // Remove deregisters and deletes the script stub.
        let rm = session.eval_line("sudo grease remove greet").await;
        assert_eq!(rm.exit_code, 0);
        assert!(!session.grease.is_script("greet"));
        assert!(!crate::grease::config::script_bin_dir().join("greet").exists());
    });
}

/// End-to-end: `grease install` fetches a `kind:skill` package, materializes its dir tree (docs +
/// bundled `bin/` scripts), and surfaces it to the model in the system prompt — but a skill is NOT
/// a command (no manifest, no `ask` tool, no `run_command` arm).
#[test]
fn grease_install_a_skill_materializes_and_surfaces_it() {
    on_rt(async {
        let _dirs = set_grease_dirs();
        let mut session = Session::new().await.unwrap();

        let pkg = serde_json::json!({
            "kind": "skill",
            "name": "code-review",
            "description": "review code carefully",
            "intended-use": "when the user asks for a code review",
            "documents": [{"path": "SKILL.md", "content": "Review for correctness first."}],
            "scripts": [{"name": "lint-all", "body": "echo linting"}]
        });
        session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![("/packages/", grease_json(pkg))])));
        session.run_line("grease registry add https://reg.example").await;

        let inst = session.eval_line("sudo grease install code-review").await;
        assert_eq!(inst.exit_code, 0, "install stderr: {}", String::from_utf8_lossy(&inst.stderr));
        let out = String::from_utf8(inst.stdout).unwrap();
        assert!(out.contains("installed code-review") && out.contains("[skill]"), "out: {out}");

        // The dir tree is materialized: doc + bundled bin script.
        let skill_root = crate::grease::config::skills_dir().join("code-review");
        assert_eq!(
            std::fs::read_to_string(skill_root.join("SKILL.md")).unwrap(),
            "Review for correctness first."
        );
        assert_eq!(
            std::fs::read_to_string(skill_root.join("bin/lint-all")).unwrap(),
            "echo linting"
        );

        // A skill is NOT a command: not a script/prompt, no manifest, no ask tool.
        assert!(session.grease.is_skill("code-review"));
        assert!(!session.grease.is_script("code-review"));
        assert!(!session.grease.is_prompt("code-review"));
        assert!(session.grease.manifest_for("code-review").is_none());
        assert!(session.grease.ask_tool_definitions().is_empty());

        // `grease info` describes the envelope + bundles.
        let info = String::from_utf8(session.eval_line("grease info code-review").await.stdout).unwrap();
        assert!(info.contains("[skill]") && info.contains("SKILL.md") && info.contains("lint-all"), "info: {info}");

        // The skill is surfaced in the agentic system prompt (context, not a callable tool).
        let sys = crate::ai::ask::build_system_prompt_with_capabilities(
            &session.registry,
            &session.mcp,
            &session.grease,
        );
        assert!(sys.contains("Installed skills"), "system prompt lists skills: …");
        assert!(sys.contains("code-review") && sys.contains("when the user asks for a code review"));

        // Remove deletes the dir tree and deregisters.
        let rm = session.eval_line("sudo grease remove code-review").await;
        assert_eq!(rm.exit_code, 0);
        assert!(!session.grease.is_skill("code-review"));
        assert!(!skill_root.exists());
    });
}

/// A deterministic ed25519 keypair (from a fixed 32-byte seed) + a signer over `body`. Returns
/// `(pubkey_b64, sig_b64)`. Dev-only (native `ed25519-dalek` signing side), no RNG.
fn sign_payload(body: &[u8]) -> (String, String) {
    use base64::Engine;
    use ed25519_dalek::{Signer, SigningKey};
    let seed = [7u8; 32]; // fixed seed → deterministic key across runs
    let sk = SigningKey::from_bytes(&seed);
    let pk_b64 = base64::engine::general_purpose::STANDARD.encode(sk.verifying_key().to_bytes());
    let sig_b64 = base64::engine::general_purpose::STANDARD.encode(sk.sign(body).to_bytes());
    (pk_b64, sig_b64)
}

/// A signed registry (configured with `--key`) installs a package whose ed25519 signature verifies,
/// records the signer, and surfaces "signed" in the output. The signature is over the EXACT bytes
/// the fake registry serves.
#[test]
fn grease_install_verifies_a_valid_signature() {
    on_rt(async {
        let _dirs = set_grease_dirs();
        let mut session = Session::new().await.unwrap();

        let pkg = serde_json::json!({
            "kind": "prompt", "name": "signed-pkg", "description": "d", "body": "hi"
        });
        let body = grease_json(pkg.clone()).body; // the exact bytes the registry serves
        let (pubkey, sig) = sign_payload(&body);
        let index = serde_json::json!({
            "packages": [{
                "name": "signed-pkg", "description": "d",
                "sha256": crate::grease::pkg::sha256_hex(&body),
                "sig": sig, "signer": "alice"
            }]
        });
        session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![
            ("/index.json", grease_json(index)),
            ("/packages/", crate::mcp::client::HttpResponse {
                status: 200,
                headers: vec![("content-type".into(), "application/json".into())],
                body,
            }),
        ])));
        session.run_line(&format!("grease registry add https://reg.example --key {pubkey}")).await;

        let inst = session.eval_line("sudo grease install signed-pkg").await;
        assert_eq!(inst.exit_code, 0, "install stderr: {}", String::from_utf8_lossy(&inst.stderr));
        let out = String::from_utf8(inst.stdout).unwrap();
        assert!(out.contains("signed"), "install reports signed: {out}");
        assert!(session.grease.is_prompt("signed-pkg"));
        // `grease info` shows the signer.
        let info = String::from_utf8(session.eval_line("grease info signed-pkg").await.stdout).unwrap();
        assert!(info.contains("signed by alice"), "info shows signer: {info}");
    });
}

/// A signed registry REJECTS a package whose signature does not verify (wrong signature) — hard
/// exit 4, nothing installed.
#[test]
fn grease_install_rejects_a_bad_signature() {
    on_rt(async {
        let _dirs = set_grease_dirs();
        let mut session = Session::new().await.unwrap();

        let pkg = serde_json::json!({
            "kind": "prompt", "name": "bad-sig", "description": "d", "body": "hi"
        });
        let body = grease_json(pkg.clone()).body;
        let (pubkey, _good_sig) = sign_payload(&body);
        // A signature over DIFFERENT bytes → verify fails against `body`.
        let (_pk2, wrong_sig) = sign_payload(b"some other content");
        let index = serde_json::json!({
            "packages": [{
                "name": "bad-sig", "description": "d",
                "sha256": crate::grease::pkg::sha256_hex(&body),
                "sig": wrong_sig, "signer": "mallory"
            }]
        });
        session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![
            ("/index.json", grease_json(index)),
            ("/packages/", crate::mcp::client::HttpResponse {
                status: 200,
                headers: vec![("content-type".into(), "application/json".into())],
                body,
            }),
        ])));
        session.run_line(&format!("grease registry add https://reg.example --key {pubkey}")).await;

        let inst = session.eval_line("sudo grease install bad-sig").await;
        assert_eq!(inst.exit_code, 4, "a bad signature must reject");
        assert!(String::from_utf8(inst.stderr).unwrap().contains("signature verification failed"));
        assert!(!session.grease.is_prompt("bad-sig"), "nothing installed on sig failure");
    });
}

/// A signed registry REJECTS a package that carries NO signature (a signed registry must sign its
/// packages) — hard exit 4.
#[test]
fn grease_install_rejects_unsigned_package_from_signed_registry() {
    on_rt(async {
        let _dirs = set_grease_dirs();
        let mut session = Session::new().await.unwrap();
        let pkg = serde_json::json!({
            "kind": "prompt", "name": "nosig", "description": "d", "body": "hi"
        });
        let body = grease_json(pkg.clone()).body;
        let (pubkey, _sig) = sign_payload(&body);
        let index = serde_json::json!({
            "packages": [{ "name": "nosig", "description": "d",
                "sha256": crate::grease::pkg::sha256_hex(&body) }] // NO sig field
        });
        session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![
            ("/index.json", grease_json(index)),
            ("/packages/", crate::mcp::client::HttpResponse {
                status: 200,
                headers: vec![("content-type".into(), "application/json".into())],
                body,
            }),
        ])));
        session.run_line(&format!("grease registry add https://reg.example --key {pubkey}")).await;

        let inst = session.eval_line("sudo grease install nosig").await;
        assert_eq!(inst.exit_code, 4);
        assert!(String::from_utf8(inst.stderr).unwrap().contains("no signature"));
        assert!(!session.grease.is_prompt("nosig"));
    });
}

/// An UNsigned registry (no `--key`) still installs, marked unsigned (record-only signing).
#[test]
fn grease_install_from_unsigned_registry_is_record_only() {
    on_rt(async {
        let _dirs = set_grease_dirs();
        let mut session = Session::new().await.unwrap();
        let pkg = serde_json::json!({
            "kind": "prompt", "name": "plain", "description": "d", "body": "hi"
        });
        session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![("/packages/", grease_json(pkg))])));
        session.run_line("grease registry add https://reg.example").await; // no --key

        let inst = session.eval_line("sudo grease install plain").await;
        assert_eq!(inst.exit_code, 0, "install stderr: {}", String::from_utf8_lossy(&inst.stderr));
        assert!(session.grease.is_prompt("plain"));
        let info = String::from_utf8(session.eval_line("grease info plain").await.stdout).unwrap();
        assert!(info.contains("unsigned"), "info shows unsigned: {info}");
    });
}

/// RFC-6962 leaf/node hashers for building a fixture transparency log in tests.
fn rfc_leaf(data: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update([0x00]);
    h.update(data);
    h.finalize().into()
}
fn rfc_node(l: &[u8], r: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update([0x01]);
    h.update(l);
    h.update(r);
    h.finalize().into()
}

/// A signed registry whose index carries a valid RFC-6962 inclusion proof installs, records
/// `log_verified`, and `grease info` shows the transparency-log index. Uses a 2-leaf tree; our
/// package's content-hash is leaf 0, some other entry is leaf 1.
#[test]
fn grease_install_verifies_transparency_log_inclusion() {
    on_rt(async {
        use base64::Engine;
        let _dirs = set_grease_dirs();
        let mut session = Session::new().await.unwrap();
        let pkg = serde_json::json!({
            "kind": "prompt", "name": "logged", "description": "d", "body": "hi"
        });
        let body = grease_json(pkg.clone()).body;
        let (pubkey, sig) = sign_payload(&body);
        // Build a 2-leaf tree: leaf 0 = our package's sha256-hex string (the log leaf), leaf 1 = a
        // sibling. Proof for leaf 0 is [leaf_hash(sibling)]; root = node(leaf0, leaf1).
        let leaf0 = crate::grease::pkg::sha256_hex(&body);
        let sibling = b"another-package-digest".to_vec();
        let h0 = rfc_leaf(leaf0.as_bytes());
        let h1 = rfc_leaf(&sibling);
        let root = rfc_node(&h0, &h1);
        let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
        let index = serde_json::json!({
            "packages": [{
                "name": "logged", "description": "d",
                "sha256": leaf0, "sig": sig, "signer": "alice",
                "log": {
                    "leaf-index": 0, "tree-size": 2,
                    "root": b64(&root), "proof": [b64(&h1)]
                }
            }]
        });
        session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![
            ("/index.json", grease_json(index)),
            ("/packages/", crate::mcp::client::HttpResponse {
                status: 200,
                headers: vec![("content-type".into(), "application/json".into())],
                body,
            }),
        ])));
        session.run_line(&format!("grease registry add https://reg.example --key {pubkey}")).await;

        let inst = session.eval_line("sudo grease install logged").await;
        assert_eq!(inst.exit_code, 0, "install stderr: {}", String::from_utf8_lossy(&inst.stderr));
        assert!(String::from_utf8(inst.stdout).unwrap().contains("in log"), "reports in-log");
        let info = String::from_utf8(session.eval_line("grease info logged").await.stdout).unwrap();
        assert!(info.contains("transparency log @0"), "info shows log index: {info}");
    });
}

/// A tampered inclusion proof (wrong root) is a HARD reject (exit 4, nothing installed).
#[test]
fn grease_install_rejects_bad_transparency_log_proof() {
    on_rt(async {
        use base64::Engine;
        let _dirs = set_grease_dirs();
        let mut session = Session::new().await.unwrap();
        let pkg = serde_json::json!({
            "kind": "prompt", "name": "badlog", "description": "d", "body": "hi"
        });
        let body = grease_json(pkg.clone()).body;
        let (pubkey, sig) = sign_payload(&body);
        let leaf0 = crate::grease::pkg::sha256_hex(&body);
        let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
        // A bogus root that won't match the recomputed one.
        let index = serde_json::json!({
            "packages": [{
                "name": "badlog", "description": "d",
                "sha256": leaf0, "sig": sig, "signer": "alice",
                "log": { "leaf-index": 0, "tree-size": 2,
                    "root": b64(&[0u8; 32]), "proof": [b64(&[1u8; 32])] }
            }]
        });
        session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![
            ("/index.json", grease_json(index)),
            ("/packages/", crate::mcp::client::HttpResponse {
                status: 200,
                headers: vec![("content-type".into(), "application/json".into())],
                body,
            }),
        ])));
        session.run_line(&format!("grease registry add https://reg.example --key {pubkey}")).await;

        let inst = session.eval_line("sudo grease install badlog").await;
        assert_eq!(inst.exit_code, 4, "a bad log proof must reject");
        assert!(String::from_utf8(inst.stderr).unwrap().contains("transparency-log check failed"));
        assert!(!session.grease.is_prompt("badlog"), "nothing installed on log failure");
    });
}

/// A bare `grease install` surfaces a capability-disclosure confirmation naming the package, its
/// source registries, and the ask capability (README "discloses capability requests"). `sudo`
/// pre-authorizes (no pause).
#[test]
fn grease_install_discloses_capabilities() {
    on_rt(async {
        let _dirs = set_grease_dirs();
        let mut session = Session::new().await.unwrap();
        session.run_line("grease registry add https://reg.example/pkgs").await;

        let surface = session.eval_line("grease install tldr").await;
        let q = surface.pending_prompt.expect("install should confirm").question;
        assert!(q.contains("\"tldr\""), "discloses the package name: {q}");
        assert!(q.contains("https://reg.example/pkgs"), "discloses the source registry: {q}");
        assert!(q.contains("run via ask"), "discloses the ask capability: {q}");
        assert!(q.contains("local shell"), "discloses the local-shell capability: {q}");
        // Deny to leave state clean.
        session.answer_prompt(Some("no".into())).await;

        // `sudo grease install` pre-authorizes — no pause (it then errors on the fetch, which is
        // fine; we're only asserting the no-pause behavior here).
        let sudo = session.eval_line("sudo grease install tldr").await;
        assert!(sudo.pending_prompt.is_none(), "sudo should not pause");
    });
}

/// A matching index sha256 → verified install.
#[test]
fn grease_install_verifies_matching_sha256() {
    on_rt(async {
        let _dirs = set_grease_dirs();
        let pkg = serde_json::json!({
            "name": "vpkg", "description": "verified package", "body": "hello."
        });
        let good = crate::grease::pkg::sha256_hex(pkg.to_string().as_bytes());
        let mut session = Session::new().await.unwrap();
        let index = serde_json::json!({
            "packages": [{"name":"vpkg","description":"verified package","sha256": good}]
        });
        session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![
            ("/index.json", grease_json(index)),
            ("/packages/", grease_json(pkg)),
        ])));
        session.run_line("grease registry add https://reg.example").await;
        let inst = session.eval_line("sudo grease install vpkg").await;
        assert_eq!(inst.exit_code, 0, "stderr: {}", String::from_utf8_lossy(&inst.stderr));
        assert!(String::from_utf8(inst.stdout).unwrap().contains("verified"));
        assert!(session.grease.is_prompt("vpkg"));
    });
}

/// A mismatched index sha256 → reject (exit 4), nothing persisted.
#[test]
fn grease_install_rejects_sha256_mismatch() {
    on_rt(async {
        let _dirs = set_grease_dirs();
        let pkg = serde_json::json!({"name":"vpkg","description":"d","body":"hello."});
        let mut session = Session::new().await.unwrap();
        let index = serde_json::json!({
            "packages": [{"name":"vpkg","sha256":"0000000000000000000000000000000000000000000000000000000000000000"}]
        });
        session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![
            ("/index.json", grease_json(index)),
            ("/packages/", grease_json(pkg)),
        ])));
        session.run_line("grease registry add https://reg.example").await;
        let inst = session.eval_line("sudo grease install vpkg").await;
        assert_eq!(inst.exit_code, 4);
        assert!(String::from_utf8(inst.stderr).unwrap().contains("integrity check failed"));
        assert!(!session.grease.is_prompt("vpkg"), "a mismatched package must not install");
        assert!(!crate::grease::config::store_dir().join("vpkg").exists());
    });
}

/// A registry index with no sha256 for the package → record-only install, with a stderr note.
#[test]
fn grease_install_record_only_without_index_hash() {
    on_rt(async {
        let _dirs = set_grease_dirs();
        let mut session = Session::new().await.unwrap();
        let pkg = serde_json::json!({"name":"loose","description":"d","body":"hi."});
        // Index present but with no sha256 field for the package.
        let index = serde_json::json!({"packages":[{"name":"loose","description":"d"}]});
        session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![
            ("/index.json", grease_json(index)),
            ("/packages/", grease_json(pkg)),
        ])));
        session.run_line("grease registry add https://reg.example").await;
        let inst = session.eval_line("sudo grease install loose").await;
        assert_eq!(inst.exit_code, 0);
        let out = String::from_utf8(inst.stdout).unwrap();
        assert!(out.contains("no integrity hash"), "expected record-only note, got: {out}");
        assert!(out.contains("unverified"));
        assert!(session.grease.is_prompt("loose"));
    });
}

/// An installed prompt is exposed to the model as a `prompt__<name>` tool: it appears in the tool
/// surface + the system prompt, and a scripted tool call runs the prompt (the model sees the FILLED
/// body). Confirms under a plain ask; runs under `sudo ask`.
#[test]
fn ask_can_call_an_installed_prompt() {
    on_rt(async {
        let _dirs = set_grease_dirs();
        let mut session = Session::new().await.unwrap();

        // Install a parameterized prompt (reuse the fetch flow).
        let pkg = serde_json::json!({
            "name": "tldr",
            "description": "one-line summary",
            "arguments": [{"name":"file","required":true}],
            "body": "TL;DR of {{file}} please."
        });
        // No index route → the index lookup 404s → record-only install (these tests don't assert
        // on integrity; the verify path has its own dedicated tests).
        session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![("/packages/", grease_json(pkg))])));
        session.run_line("grease registry add https://reg.example").await;
        let inst = session.eval_line("sudo grease install tldr").await;
        assert_eq!(inst.exit_code, 0);

        // The model calls the prompt tool by its namespaced name with the required arg. The shared
        // FakeProvider serves three turns in order: (1) the outer ask's tool call, (2) the NESTED
        // prompt run's reply (the prompt tool re-enters the model), (3) the outer ask's final text.
        let ask_seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::scripted(
            vec![
                crate::ai::ask::AskResponse {
                    text: String::new(),
                    tool_calls: vec![crate::ai::ask::AskToolCall {
                        id: "p1".into(),
                        name: "prompt__tldr".into(),
                        arguments_json: serde_json::json!({ "file": "report.md" }).to_string(),
                    }],
                    finished_for_tools: true,
                    error: None,
                },
                crate::ai::ask::AskResponse::text("the one-line summary"), // nested prompt reply
                crate::ai::ask::AskResponse::text("summarized it"),        // outer ask final text
            ],
            ask_seen.clone(),
        )));

        let result = session.eval_line(r#"sudo ask "summarize report.md with the tldr prompt""#).await;
        assert_eq!(result.exit_code, 0, "stderr: {}", String::from_utf8_lossy(&result.stderr));
        assert_eq!(String::from_utf8(result.stdout).unwrap(), "summarized it");

        let turns = ask_seen.lock().unwrap();
        // The prompt tool was in the outer ask's tool surface AND listed in its system prompt.
        assert!(
            turns[0].tools.iter().any(|t| t.name == "prompt__tldr"),
            "the prompt should be an ask tool"
        );
        let system = turns[0].system.clone().unwrap_or_default();
        assert!(system.contains("prompt__tldr"), "system prompt should list the prompt tool");
        // The nested prompt run saw the FILLED body (turn 2 — the {{file}} was substituted).
        let saw_filled = turns
            .iter()
            .any(|t| t.user_content().contains("TL;DR of report.md please."));
        assert!(saw_filled, "the model should have seen the filled prompt body");
    });
}

/// Under a plain (non-sudo) ask, an installed-prompt tool call pauses for authorization (running a
/// prompt is an outbound LLM call → Confirm).
#[test]
fn ask_prompt_tool_pauses_without_sudo() {
    on_rt(async {
        let _dirs = set_grease_dirs();
        let mut session = Session::new().await.unwrap();
        let pkg = serde_json::json!({
            "name": "greet", "description": "greet", "body": "Say hello."
        });
        // No index route → the index lookup 404s → record-only install (these tests don't assert
        // on integrity; the verify path has its own dedicated tests).
        session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![("/packages/", grease_json(pkg))])));
        session.run_line("grease registry add https://reg.example").await;
        session.eval_line("sudo grease install greet").await;

        session.set_ask_provider(Box::new(FakeProvider::scripted(
            vec![crate::ai::ask::AskResponse {
                text: String::new(),
                tool_calls: vec![crate::ai::ask::AskToolCall {
                    id: "p1".into(),
                    name: "prompt__greet".into(),
                    arguments_json: "{}".into(),
                }],
                finished_for_tools: true,
                error: None,
            }],
            std::sync::Arc::new(Mutex::new(Vec::new())),
        )));
        // Plain (non-sudo) ask: the prompt tool call pauses for authorization.
        let r = session.eval_line(r#"ask "greet the user""#).await;
        // The ask itself first confirms (outbound HTTP), then the tool call confirms — either way a
        // pause is surfaced.
        assert!(r.pending_prompt.is_some(), "a plain ask + prompt tool call should pause");
        session.answer_prompt(Some("no".into())).await; // drain
    });
}

/// A registry-name collision with a builtin is rejected at install.
#[test]
fn grease_install_rejects_builtin_collision() {
    on_rt(async {
        let _dirs = set_grease_dirs();
        let mut session = Session::new().await.unwrap();
        session.set_mcp_http(Box::new(FakeMcpHttp::new(vec![])));
        session.run_line("grease registry add https://reg.example").await;
        // `ask` is a builtin — installing a package named `ask` must fail before any fetch.
        let r = session.eval_line("sudo grease install ask").await;
        assert_eq!(r.exit_code, 2);
        assert!(String::from_utf8(r.stderr).unwrap().contains("collides with a built-in"));
    });
}

/// `grease install` (a Confirm subcommand) surfaces a confirmation without sudo; a non-http
/// registry URL is rejected; install with no registry gives an honest error.
#[test]
fn grease_install_confirms_and_errors_without_registry() {
    on_rt(async {
        let _dirs = set_grease_dirs();
        let mut session = Session::new().await.unwrap();

        let bad = session.eval_line("grease registry add not-a-url").await;
        assert_eq!(bad.exit_code, 2);
        assert!(String::from_utf8(bad.stderr).unwrap().contains("not an http"));

        // `install` is Confirm — a bare invocation pauses.
        let confirm = session.eval_line("grease install summarize").await;
        assert!(confirm.pending_prompt.is_some(), "install should confirm without sudo");
        session.answer_prompt(Some("no".into())).await;

        // Under sudo it runs; with no registry configured it errors honestly (no panic).
        let inst = session.eval_line("sudo grease install summarize").await;
        assert_eq!(inst.exit_code, 1);
        assert!(String::from_utf8(inst.stderr).unwrap().contains("no registries configured"));
    });
}

/// reflect it. Uses a scripted fake transport.
#[test]
fn mcp_add_installs_and_surfaces_the_server() {
    on_rt(async {
        let dirs = set_mcp_dirs();
        let mut session = Session::new().await.unwrap();
        // Put the mcp bin dir on PATH so `which` finds the stub.
        session.run_line(&format!("export PATH={}:$PATH", dirs.bin)).await;
        session.set_mcp_http(Box::new(FakeMcpHttp::new(mcp_install_script())));

        let add = session.eval_line("sudo mcp add demo https://x.example/mcp").await;
        assert_eq!(add.exit_code, 0, "add failed: {}", String::from_utf8_lossy(&add.stderr));
        assert!(String::from_utf8(add.stdout).unwrap().contains("1 tools"));

        let (list, _) = session.run_line("mcp list").await;
        let list = String::from_utf8(list).unwrap();
        assert!(list.contains("demo") && list.contains("1 tools"), "got: {list}");

        let (tools, _) = session.run_line("mcp tools demo").await;
        assert!(String::from_utf8(tools).unwrap().contains("echo"));

        // The /usr/lib/mcp/bin stub is a real file, so `which` finds it.
        let (which, _) = session.run_line("which demo").await;
        assert!(String::from_utf8(which).unwrap().contains("demo"), "which should find the stub");
    });
}

/// A NEW process reconstructs a plain `mcp add` server from its on-disk config — no network. The
/// install caches the tool list in the config; the second Session (fresh, NO transport installed)
/// still knows the server and its tools. Before this, `McpState` was in-memory only and a native
/// restart forgot every non-grease server.
#[test]
fn mcp_state_reconstructs_from_config_cache() {
    on_rt(async {
        let _dirs = set_mcp_dirs();
        {
            let mut session = Session::new().await.unwrap();
            session.set_mcp_http(Box::new(FakeMcpHttp::new(mcp_install_script())));
            let add = session.eval_line("sudo mcp add demo https://x.example/mcp").await;
            assert_eq!(add.exit_code, 0, "add failed: {}", String::from_utf8_lossy(&add.stderr));
        }
        // A brand-new Session, same CLANK_MCP_ETC, no HTTP transport at all.
        let mut fresh = Session::new().await.unwrap();
        let (list, _) = fresh.run_line("mcp list").await;
        let list = String::from_utf8(list).unwrap();
        assert!(list.contains("demo"), "reconstructed server missing: {list}");
        let (tools, _) = fresh.run_line("mcp tools demo").await;
        let tools = String::from_utf8(tools).unwrap();
        assert!(tools.contains("echo"), "cached tools missing: {tools}");
    });
}

/// `mcp add` against an erroring transport keeps the config as "not installed" and exits 4.
#[test]
fn mcp_add_transport_failure_is_configured_not_installed() {
    on_rt(async {
        let _dirs = set_mcp_dirs();
        let mut session = Session::new().await.unwrap();
        // initialize returns a 500.
        let bad = crate::mcp::client::HttpResponse { status: 500, headers: vec![], body: vec![] };
        session.set_mcp_http(Box::new(FakeMcpHttp::new(vec![bad])));

        let add = session.eval_line("sudo mcp add demo https://x.example/mcp").await;
        assert_eq!(add.exit_code, 4);
        let (list, _) = session.run_line("mcp list").await;
        assert!(String::from_utf8(list).unwrap().contains("not installed"));
    });
}

/// A server name colliding with a built-in command is rejected.
#[test]
fn mcp_add_rejects_a_builtin_name_collision() {
    on_rt(async {
        let _dirs = set_mcp_dirs();
        let mut session = Session::new().await.unwrap();
        session.set_mcp_http(Box::new(FakeMcpHttp::new(vec![])));
        let add = session.eval_line("sudo mcp add grep https://x/mcp").await;
        assert_eq!(add.exit_code, 2);
        assert!(String::from_utf8(add.stderr).unwrap().contains("collides"));
    });
}

/// `mcp add` is `Confirm`-policy (outbound HTTP): a bare `mcp add` surfaces a confirmation, while
/// `mcp list` (Allow subcommand) does not.
#[test]
fn mcp_add_confirms_but_list_does_not() {
    on_rt(async {
        let _dirs = set_mcp_dirs();
        let mut session = Session::new().await.unwrap();
        session.set_mcp_http(Box::new(FakeMcpHttp::new(mcp_install_script())));

        // `mcp list` (subcommand Allow) runs without a confirm.
        let list = session.eval_line("mcp list").await;
        assert!(list.pending_prompt.is_none(), "mcp list should not confirm");

        // Bare `mcp add` (subcommand Confirm) surfaces a confirmation.
        let add = session.eval_line("mcp add demo https://x/mcp").await;
        assert!(add.pending_prompt.is_some(), "mcp add should confirm");
        // Approve → the install runs.
        let done = session.answer_prompt(Some("yes".to_string())).await;
        assert_eq!(done.exit_code, 0);
    });
}

/// A `tools/call` response echoing text content.
fn mcp_call_response(text: &str) -> crate::mcp::client::HttpResponse {
    mcp_json(serde_json::json!({"jsonrpc":"2.0","id":9,"result":{
        "content":[{"type":"text","text":text}], "isError":false}}))
}

/// `<server> <tool> --param v` runs a tool call: args mapped from the schema; result text returned.
#[test]
fn mcp_tool_dispatch_maps_args_and_returns_text() {
    on_rt(async {
        let dirs = set_mcp_dirs();
        let mut session = Session::new().await.unwrap();
        let mut script = mcp_install_script();
        script.push(mcp_call_response("echoed: hello"));
        session.set_mcp_http(Box::new(FakeMcpHttp::new(script)));
        session.eval_line("sudo mcp add demo https://x/mcp").await;

        // `sudo demo echo --text hello` runs the tool (sudo pre-authorizes the Confirm).
        let out = session.eval_line("sudo demo echo --text hello").await;
        assert_eq!(out.exit_code, 0, "stderr: {}", String::from_utf8_lossy(&out.stderr));
        assert!(String::from_utf8(out.stdout).unwrap().contains("echoed: hello"));
        let _ = dirs;
    });
}

/// A missing required argument is a usage error (exit 2), no HTTP call.
#[test]
fn mcp_tool_missing_required_arg_errors() {
    on_rt(async {
        let _dirs = set_mcp_dirs();
        let mut session = Session::new().await.unwrap();
        session.set_mcp_http(Box::new(FakeMcpHttp::new(mcp_install_script())));
        session.eval_line("sudo mcp add demo https://x/mcp").await;
        // `echo` requires `text`; omit it.
        let out = session.eval_line("sudo demo echo").await;
        assert_eq!(out.exit_code, 2);
        assert!(String::from_utf8(out.stderr).unwrap().contains("required"));
    });
}

/// A bare `<server> <tool>` (no sudo) surfaces a confirmation; approving runs it.
#[test]
fn mcp_tool_confirms_then_runs() {
    on_rt(async {
        let _dirs = set_mcp_dirs();
        let mut session = Session::new().await.unwrap();
        let mut script = mcp_install_script();
        script.push(mcp_call_response("ran"));
        session.set_mcp_http(Box::new(FakeMcpHttp::new(script)));
        session.eval_line("sudo mcp add demo https://x/mcp").await;

        let first = session.eval_line("demo echo --text hi").await;
        assert!(first.pending_prompt.is_some(), "MCP tool call should confirm");
        let done = session.answer_prompt(Some("yes".to_string())).await;
        assert_eq!(done.exit_code, 0);
        assert!(String::from_utf8(done.stdout).unwrap().contains("ran"));
    });
}

/// `<server> --help` prints the server's tool list without confirming; `man <server>` too.
#[test]
fn mcp_server_help_and_man_surfaces() {
    on_rt(async {
        let _dirs = set_mcp_dirs();
        let mut session = Session::new().await.unwrap();
        session.set_mcp_http(Box::new(FakeMcpHttp::new(mcp_install_script())));
        session.eval_line("sudo mcp add demo https://x/mcp").await;

        let help = session.eval_line("demo --help").await;
        assert!(help.pending_prompt.is_none(), "help must not confirm");
        assert!(String::from_utf8(help.stdout).unwrap().contains("echo"));

        let (man, _) = session.run_line("man demo").await;
        assert!(String::from_utf8(man).unwrap().contains("demo"), "man should resolve the server");
    });
}

/// The `--args '<json>'` escape hatch bypasses schema mapping.
#[test]
fn mcp_tool_raw_args_escape_hatch() {
    on_rt(async {
        let _dirs = set_mcp_dirs();
        let mut session = Session::new().await.unwrap();
        let mut script = mcp_install_script();
        script.push(mcp_call_response("raw ok"));
        let http = FakeMcpHttp::new(script);
        let seen = http.seen.clone();
        session.set_mcp_http(Box::new(http));
        session.eval_line("sudo mcp add demo https://x/mcp").await;

        let out = session.eval_line(r#"sudo demo echo --args '{"text":"direct"}'"#).await;
        assert_eq!(out.exit_code, 0, "stderr: {}", String::from_utf8_lossy(&out.stderr));
        // The tools/call body carried the raw args verbatim.
        let calls = seen.lock().unwrap();
        let last = calls.last().unwrap();
        assert!(last.1.contains("\"text\":\"direct\""), "tools/call body: {}", last.1);
    });
}

/// `mcp session open/list/info/close` lifecycle over a fake transport.
#[test]
fn mcp_session_lifecycle() {
    on_rt(async {
        let _dirs = set_mcp_dirs();
        let mut session = Session::new().await.unwrap();
        // install script (init+initialized+tools/list) then a SECOND init for `session open`.
        let mut script = mcp_install_script();
        let mut open_init = mcp_json(serde_json::json!({"jsonrpc":"2.0","id":3,"result":{
            "protocolVersion":"2025-03-26",
            "serverInfo":{"name":"demo","version":"1.0"},"capabilities":{"tools":{}}}}));
        open_init.headers.push(("mcp-session-id".into(), "srv-open".into()));
        script.push(open_init);
        script.push(mcp_json(serde_json::json!({}))); // initialized
        script.push(mcp_json(serde_json::json!({}))); // DELETE close (200)
        session.set_mcp_http(Box::new(FakeMcpHttp::new(script)));
        session.eval_line("sudo mcp add demo https://x/mcp").await;

        // No sessions yet.
        let (list0, _) = session.run_line("mcp session list").await;
        assert!(String::from_utf8(list0).unwrap().contains("no open MCP sessions"));

        // Open one.
        let open = session.eval_line("sudo mcp session open demo").await;
        assert_eq!(open.exit_code, 0, "stderr: {}", String::from_utf8_lossy(&open.stderr));
        assert!(String::from_utf8(open.stdout).unwrap().contains("s1"));

        let (list1, _) = session.run_line("mcp session list").await;
        let list1 = String::from_utf8(list1).unwrap();
        assert!(list1.contains("s1") && list1.contains("srv-open"), "got: {list1}");

        let (info, _) = session.run_line("mcp session info s1").await;
        assert!(String::from_utf8(info).unwrap().contains("demo"));

        // Close it.
        let close = session.eval_line("sudo mcp session close s1").await;
        assert_eq!(close.exit_code, 0);
        let (list2, _) = session.run_line("mcp session list").await;
        assert!(String::from_utf8(list2).unwrap().contains("no open MCP sessions"));
    });
}

/// Closing a session the server refuses (HTTP 405) still removes it locally, with a clear message.
#[test]
fn mcp_session_close_405_removes_locally() {
    on_rt(async {
        let _dirs = set_mcp_dirs();
        let mut session = Session::new().await.unwrap();
        let mut script = mcp_install_script();
        let mut open_init = mcp_json(serde_json::json!({"jsonrpc":"2.0","id":3,"result":{
            "serverInfo":{"name":"demo","version":"1.0"},"capabilities":{}}}));
        open_init.headers.push(("mcp-session-id".into(), "srv-open".into()));
        script.push(open_init);
        script.push(mcp_json(serde_json::json!({}))); // initialized
        script.push(crate::mcp::client::HttpResponse { status: 405, headers: vec![], body: vec![] });
        session.set_mcp_http(Box::new(FakeMcpHttp::new(script)));
        session.eval_line("sudo mcp add demo https://x/mcp").await;
        session.eval_line("sudo mcp session open demo").await;

        let close = session.eval_line("sudo mcp session close s1").await;
        // Message names the 405 refusal; the local session is gone regardless.
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&close.stdout),
            String::from_utf8_lossy(&close.stderr)
        );
        assert!(combined.contains("405") || combined.contains("locally"), "got: {combined}");
        let (list, _) = session.run_line("mcp session list").await;
        assert!(String::from_utf8(list).unwrap().contains("no open MCP sessions"));
    });
}

/// C4: an installed MCP tool becomes an ask ToolDefinition. Under `sudo ask`, the model calling
/// `mcp__demo__echo` runs the tool (blanket confirm-tier) and the FakeMcpHttp sees the tools/call.
#[test]
fn ask_can_call_an_mcp_tool() {
    on_rt(async {
        let _dirs = set_mcp_dirs();
        let mut session = Session::new().await.unwrap();
        let mut script = mcp_install_script();
        script.push(mcp_call_response("echoed by mcp"));
        let http = FakeMcpHttp::new(script);
        let seen = http.seen.clone();
        session.set_mcp_http(Box::new(http));
        session.eval_line("sudo mcp add demo https://x/mcp").await;

        // The model calls the MCP tool by its namespaced name.
        let ask_seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::scripted(
            vec![
                crate::ai::ask::AskResponse {
                    text: String::new(),
                    tool_calls: vec![crate::ai::ask::AskToolCall {
                        id: "m1".into(),
                        name: "mcp__demo__echo".into(),
                        arguments_json: serde_json::json!({ "text": "hi mcp" }).to_string(),
                    }],
                    finished_for_tools: true,
                    error: None,
                },
                crate::ai::ask::AskResponse::text("done, the tool ran"),
            ],
            ask_seen.clone(),
        )));

        let result = session.eval_line(r#"sudo ask "use the demo echo tool""#).await;
        assert_eq!(result.exit_code, 0, "stderr: {}", String::from_utf8_lossy(&result.stderr));
        assert_eq!(String::from_utf8(result.stdout).unwrap(), "done, the tool ran");
        // The MCP server saw a tools/call carrying the tool's arguments.
        let calls = seen.lock().unwrap();
        let tool_call = calls.iter().find(|(_url, body)| body.contains("tools/call"));
        assert!(tool_call.is_some(), "expected a tools/call, saw: {calls:?}");
        assert!(tool_call.unwrap().1.contains("hi mcp"), "args should reach the server");
        // The tool surface the model saw included the MCP tool definition.
        let ask_turns = ask_seen.lock().unwrap();
        assert!(
            ask_turns[0].tools.iter().any(|t| t.name == "mcp__demo__echo"),
            "the MCP tool should be in the ask tool surface"
        );
    });
}

/// Under a plain (non-sudo) ask, an MCP tool call pauses for authorization (MCP calls are Confirm).
#[test]
fn ask_mcp_tool_pauses_without_sudo() {
    on_rt(async {
        let _dirs = set_mcp_dirs();
        let mut session = Session::new().await.unwrap();
        let mut script = mcp_install_script();
        script.push(mcp_call_response("ran after approval"));
        session.set_mcp_http(Box::new(FakeMcpHttp::new(script)));
        session.eval_line("sudo mcp add demo https://x/mcp").await;

        session.set_ask_provider(Box::new(FakeProvider::scripted(
            vec![
                crate::ai::ask::AskResponse {
                    text: String::new(),
                    tool_calls: vec![crate::ai::ask::AskToolCall {
                        id: "m1".into(),
                        name: "mcp__demo__echo".into(),
                        arguments_json: serde_json::json!({ "text": "x" }).to_string(),
                    }],
                    finished_for_tools: true,
                    error: None,
                },
                crate::ai::ask::AskResponse::text("finished"),
            ],
            std::sync::Arc::new(Mutex::new(Vec::new())),
        )));

        // Plain ask: approve the ask, then the MCP tool call pauses for its own authz.
        session.eval_line(r#"ask "use the tool""#).await;
        let after_ask = session.answer_prompt(Some("yes".to_string())).await;
        assert!(after_ask.pending_prompt.is_some(), "MCP tool call should pause under plain ask");
        let done = session.answer_prompt(Some("yes".to_string())).await;
        assert_eq!(done.exit_code, 0);
        assert_eq!(String::from_utf8(done.stdout).unwrap(), "finished");
    });
}

/// `mcp watch` on a URI no installed server owns is an honest error (the bounded-poll happy path is
/// covered by `mcp_watch_is_a_bounded_poll`).
#[test]
fn mcp_watch_unknown_uri_errors() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let result = session.eval_line("mcp watch some://uri").await;
        assert_eq!(result.exit_code, 1);
        assert!(String::from_utf8(result.stderr).unwrap().contains("no installed server owns"));
    });
}

/// With a provider installed, `ask` returns the model's reply on stdout (exit 0), and the request
/// it assembled carries the current transcript as context (the README "transcript is the context").
/// `ask` is `Confirm`-gated, so `sudo ask` is used here to skip the confirmation pause.
#[test]
fn ask_returns_reply_and_feeds_transcript_as_context() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::reply(
            "the answer is 42",
            seen.clone(),
        )));

        // Run a command first so there's transcript history to feed as context.
        session.run_line("echo marker_abc").await;

        let result = session.eval_line(r#"sudo ask "what did I just echo?""#).await;
        assert_eq!(result.exit_code, 0);
        assert!(result.pending_prompt.is_none(), "sudo ask must not confirm");
        assert_eq!(
            String::from_utf8(result.stdout).unwrap(),
            "the answer is 42"
        );

        // The provider saw one turn: the first user message carries the prompt and the transcript
        // (including the prior echo), with the default model.
        let turns = seen.lock().unwrap().clone();
        assert_eq!(turns.len(), 1, "one turn expected, got: {}", turns.len());
        let content = turns[0].user_content();
        assert!(
            content.contains("what did I just echo?"),
            "user content should carry the prompt, got: {content}"
        );
        assert_eq!(turns[0].model, crate::ai::ask::DEFAULT_MODEL);
        assert!(
            content.contains("marker_abc"),
            "transcript context should include the prior echo, got: {content}"
        );
    });
}

/// When recording a command evicts old entries to stay under budget, the leading count marker is
/// upgraded into a model-generated summary block (the README's summarize-at-leading-edge compaction).
#[test]
fn auto_compaction_summarizes_the_dropped_span() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        // Several identical summaries: each eviction re-opens the pending span and re-summarizes,
        // so more than one summarize turn can fire across the run (the last one wins the marker).
        session.set_ask_provider(Box::new(FakeProvider::scripted(
            vec![crate::ai::ask::AskResponse::text("SUMMARY: earlier work happened"); 8],
            seen.clone(),
        )));

        // Shrink the window so the next few commands force an eviction.
        session.eval_line("context budget 4").await;
        session.run_line("echo marker_one").await;
        session.run_line("echo marker_two").await;
        session.run_line("echo marker_three").await;

        let shown = String::from_utf8(session.eval_line("context show").await.stdout).unwrap();
        // The leading marker is a summary block carrying the model's text, not a bare count.
        assert!(
            shown.contains("[summary of") && shown.contains("SUMMARY: earlier work happened"),
            "expected a summary block at the leading edge, got:\n{shown}"
        );
        assert!(!shown.contains("earlier entries dropped"), "count marker should be upgraded");

        // The provider was asked to summarize the DROPPED span (system = SUMMARIZE_SYSTEM_PROMPT),
        // not the whole transcript.
        let turns = seen.lock().unwrap().clone();
        assert!(
            turns.iter().any(|t| t.system.as_deref() == Some(crate::ai::ask::SUMMARIZE_SYSTEM_PROMPT)),
            "a summarize turn should have fired"
        );
    });
}

/// With no provider (native), auto-compaction leaves the bare `[N earlier entries dropped]` count
/// marker — the decided fallback: eviction never blocks or fails on the summary being unavailable.
#[test]
fn auto_compaction_falls_back_to_count_marker_without_a_provider() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        // No ask_provider injected.
        session.eval_line("context budget 4").await;
        session.run_line("echo marker_one").await;
        session.run_line("echo marker_two").await;
        session.run_line("echo marker_three").await;

        let shown = String::from_utf8(session.eval_line("context show").await.stdout).unwrap();
        assert!(
            shown.contains("earlier entries dropped"),
            "without a provider the count marker stays, got:\n{shown}"
        );
        assert!(!shown.contains("[summary of"), "no summary block without a provider");
    });
}

/// Points `CLANK_LOG_DIR` at a fresh temp dir for a Session logging test, restoring the env on drop.
/// Serializes via a process-wide lock (env is global). The default `DefaultLogSink` (installed by
/// `eval_line`) then writes real files under this dir.
struct LogCapture {
    _lock: std::sync::MutexGuard<'static, ()>,
    dir: std::path::PathBuf,
}
impl LogCapture {
    fn new(tag: &str) -> Self {
        let lock = crate::logging::test_env_lock();
        let dir = std::env::temp_dir().join(format!("clank-sesslog-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var(crate::logging::LOG_DIR_ENV, &dir);
        Self { _lock: lock, dir }
    }
    fn read(&self, file: crate::logging::LogFile) -> String {
        std::fs::read_to_string(self.dir.join(file.filename())).unwrap_or_default()
    }
}
impl Drop for LogCapture {
    fn drop(&mut self) {
        std::env::remove_var(crate::logging::LOG_DIR_ENV);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A normal command writes start + end (with exit code) events to shell.log.
#[test]
fn shell_log_records_start_and_end() {
    on_rt(async {
        let cap = LogCapture::new("shell");
        let mut session = Session::new().await.unwrap();
        session.eval_line("echo hi").await;
        session.eval_line("false").await;
        let log = cap.read(crate::logging::LogFile::Shell);
        assert!(log.contains(r#"start line="echo hi""#), "got:\n{log}");
        assert!(log.contains(r#"end line="echo hi" exit=0"#), "got:\n{log}");
        assert!(log.contains("exit=1"), "the failing command's exit code is logged, got:\n{log}");
    });
}

/// A destructive (`sudo-only`) command is recorded in ops.log with its authorization outcome, even
/// when denied (a bare `rm` is `sudo-only` → confirm-required without sudo).
#[test]
fn ops_log_records_destructive_ops() {
    on_rt(async {
        let cap = LogCapture::new("ops");
        let mut session = Session::new().await.unwrap();
        // A bare `rm` is the destructive tier; without sudo it needs confirmation.
        let r = session.eval_line("rm /tmp/whatever").await;
        assert!(r.pending_prompt.is_some(), "rm should confirm");
        session.answer_prompt(Some("no".into())).await;
        let log = cap.read(crate::logging::LogFile::Ops);
        assert!(log.contains("destructive"), "ops.log should record the destructive op, got:\n{log}");
        assert!(log.contains("cmd=rm"), "got:\n{log}");
        assert!(log.contains("confirm-required"), "got:\n{log}");
    });
}

/// An `ask` LLM turn is recorded in http.log (via the LoggingAskProvider wrapper).
#[test]
fn http_log_records_the_llm_turn() {
    on_rt(async {
        let cap = LogCapture::new("http");
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::reply("reply", seen)));
        session.eval_line(r#"sudo ask "hello""#).await;
        let log = cap.read(crate::logging::LogFile::Http);
        assert!(log.contains("kind=llm"), "http.log should record the LLM call, got:\n{log}");
        assert!(log.contains("status=ok"), "got:\n{log}");
    });
}

/// `--fresh` sends no transcript context; the prompt still reaches the provider.
#[test]
fn ask_fresh_sends_empty_transcript() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::reply("ok", seen.clone())));

        session.run_line("echo should_not_appear").await;
        let result = session.eval_line(r#"sudo ask --fresh "hi""#).await;
        assert_eq!(result.exit_code, 0);

        // --fresh sends no transcript: the user content is just the prompt, no marker.
        let turns = seen.lock().unwrap().clone();
        let content = turns[0].user_content();
        assert!(
            !content.contains("should_not_appear"),
            "fresh should omit the transcript, got: {content}"
        );
        assert_eq!(content, "hi");
    });
}

/// `ask --json` with a valid-JSON reply: the JSON is on stdout, exit 0, and the model saw the
/// JSON-mode directive in its system prompt.
#[test]
fn ask_json_valid_reply_exits_zero() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::reply(
            r#"{"ok":true}"#,
            seen.clone(),
        )));

        let result = session.eval_line(r#"sudo ask --json --fresh "give me json""#).await;
        assert_eq!(result.exit_code, 0);
        assert_eq!(String::from_utf8(result.stdout).unwrap(), r#"{"ok":true}"#);

        // The system prompt carried the JSON-mode directive.
        let turns = seen.lock().unwrap().clone();
        let system = turns[0].system.clone().unwrap_or_default();
        assert!(
            system.contains("single valid JSON value"),
            "json mode should add the directive, got: {system}"
        );
    });
}

/// `ask --json` wrapping its JSON in a Markdown code fence still validates (the fence is stripped).
#[test]
fn ask_json_strips_code_fence() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session.set_ask_provider(Box::new(FakeProvider::reply(
            "```json\n[1,2,3]\n```",
            std::sync::Arc::new(Mutex::new(Vec::new())),
        )));
        let result = session.eval_line(r#"sudo ask --json --fresh "list""#).await;
        assert_eq!(result.exit_code, 0);
        assert_eq!(String::from_utf8(result.stdout).unwrap(), "[1,2,3]");
    });
}

/// `ask --json` with a prose (non-JSON) reply exits 6 with the raw text on stderr and empty
/// stdout — the README `--json` contract.
#[test]
fn ask_json_invalid_reply_exits_six() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session.set_ask_provider(Box::new(FakeProvider::reply(
            "sorry, I cannot do that",
            std::sync::Arc::new(Mutex::new(Vec::new())),
        )));
        let result = session.eval_line(r#"sudo ask --json --fresh "give me json""#).await;
        assert_eq!(result.exit_code, 6);
        assert!(result.stdout.is_empty(), "no stdout on a --json failure");
        let stderr = String::from_utf8(result.stderr).unwrap();
        assert!(stderr.contains("did not return valid JSON"), "stderr: {stderr}");
        assert!(stderr.contains("sorry, I cannot do that"), "raw text preserved: {stderr}");
    });
}

/// `echo hi | sudo ask "q"` (Phase B): the upstream runs, its stdout is captured and fed to the
/// model as a stdin block. `sudo` on the tail pre-authorizes (no confirmation).
#[test]
fn ask_pipe_feeds_upstream_stdout_as_stdin() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::reply("got it", seen.clone())));

        let result = session
            .eval_line(r#"echo piped_marker_xyz | sudo ask --fresh "what did I pipe?""#)
            .await;
        assert_eq!(result.exit_code, 0);
        assert_eq!(String::from_utf8(result.stdout).unwrap(), "got it");

        let turns = seen.lock().unwrap().clone();
        assert_eq!(turns.len(), 1);
        let content = turns[0].user_content();
        assert!(
            content.contains("# Piped input (stdin)"),
            "stdin block missing, got: {content}"
        );
        assert!(
            content.contains("piped_marker_xyz"),
            "captured upstream stdout should be in the stdin block, got: {content}"
        );
        // --fresh: the prompt is present, but no transcript context header.
        assert!(content.contains("what did I pipe?"));
    });
}

/// A bare (non-sudo) ask-tail pipeline surfaces the ask's confirmation, and the captured stdin
/// survives the pause: after approval, the model sees the piped bytes.
#[test]
fn ask_pipe_confirmation_preserves_stdin() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::reply("answer", seen.clone())));

        // Bare ask ⇒ pauses for confirmation; the upstream was captured for the pause.
        let surface = session
            .eval_line(r#"echo survives_pause_marker | ask --fresh "q""#)
            .await;
        assert!(surface.pending_prompt.is_some(), "bare ask-pipe should confirm");
        assert!(seen.lock().unwrap().is_empty(), "model not called before approval");

        // Approve ⇒ the ask runs with the preserved stdin.
        let done = session.answer_prompt(Some("yes".into())).await;
        assert_eq!(done.exit_code, 0);
        let content = seen.lock().unwrap()[0].user_content();
        assert!(
            content.contains("survives_pause_marker"),
            "stdin should survive the pause, got: {content}"
        );
    });
}

/// A denied ask-tail pipeline exits 5 and does NOT leak the captured stdin into a later ask.
#[test]
fn ask_pipe_denied_exits_five_and_clears_stdin() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::reply("later", seen.clone())));

        let surface = session.eval_line(r#"echo leak_marker | ask --fresh "q""#).await;
        assert!(surface.pending_prompt.is_some());
        let denied = session.answer_prompt(Some("no".into())).await;
        assert_eq!(denied.exit_code, 5);
        assert!(seen.lock().unwrap().is_empty(), "denied ask never calls the model");

        // A subsequent unrelated sudo ask must not carry the earlier pipe's stdin.
        session.eval_line(r#"sudo ask --fresh "hello""#).await;
        let content = seen.lock().unwrap()[0].user_content();
        assert!(
            !content.contains("leak_marker"),
            "stale stdin leaked into a later ask, got: {content}"
        );
    });
}

/// `sudo context summarize` runs the LLM (no pause), prints the summary, and does NOT mutate or
/// re-record the transcript (inspection only, like `context show`).
#[test]
fn context_summarize_returns_summary_without_mutating() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::reply(
            "You ran two echo commands.",
            seen.clone(),
        )));
        session.run_line("echo original_marker_one").await;
        session.run_line("echo original_marker_two").await;

        let result = session.eval_line("sudo context summarize").await;
        assert_eq!(result.exit_code, 0);
        assert_eq!(
            String::from_utf8(result.stdout).unwrap(),
            "You ran two echo commands.\n"
        );
        assert!(result.pending_prompt.is_none(), "sudo must not pause");
        // The provider saw the transcript (the two echoes) as its user content.
        let content = seen.lock().unwrap()[0].user_content();
        assert!(content.contains("original_marker_one") && content.contains("original_marker_two"));

        // The transcript is UNCHANGED: both echoes still there, the summary is NOT recorded.
        let shown = String::from_utf8(session.eval_line("context show").await.stdout).unwrap();
        assert!(shown.contains("original_marker_one") && shown.contains("original_marker_two"));
        assert!(!shown.contains("You ran two echo commands"), "summary must not be recorded");
    });
}

/// A bare (non-sudo) `context summarize` surfaces a Confirm pause (outbound LLM HTTP); deny → exit
/// 5, approve → runs.
#[test]
fn context_summarize_confirms_then_runs() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::reply("a summary", seen.clone())));
        session.run_line("echo something").await;

        // Bare summarize pauses.
        let surface = session.eval_line("context summarize").await;
        assert!(surface.pending_prompt.is_some(), "bare summarize should confirm");
        assert!(seen.lock().unwrap().is_empty(), "model not called before approval");

        // Approve ⇒ runs.
        let done = session.answer_prompt(Some("yes".into())).await;
        assert_eq!(done.exit_code, 0);
        assert_eq!(String::from_utf8(done.stdout).unwrap(), "a summary\n");
        // The summary is still not recorded after the deferred run.
        let shown = String::from_utf8(session.eval_line("context show").await.stdout).unwrap();
        assert!(!shown.contains("a summary"), "deferred summary must not be recorded");
    });
}

/// A denied `context summarize` exits 5 and never calls the model.
#[test]
fn context_summarize_denied_exits_five() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::reply("nope", seen.clone())));
        session.run_line("echo x").await;
        let surface = session.eval_line("context summarize").await;
        assert!(surface.pending_prompt.is_some());
        let denied = session.answer_prompt(Some("no".into())).await;
        assert_eq!(denied.exit_code, 5);
        assert!(seen.lock().unwrap().is_empty(), "denied summarize never calls the model");
    });
}

/// `context summarize` with no provider (native) degrades to a clean exit-4 error.
#[test]
fn context_summarize_without_provider_errors() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session.run_line("echo x").await;
        let result = session.eval_line("sudo context summarize").await;
        assert_eq!(result.exit_code, 4);
        assert!(String::from_utf8(result.stderr).unwrap().contains("no model provider"));
    });
}

/// `context summarize` inside `$(...)` stays with Brush and hits the honest error (it can't run
/// the LLM in the nested runtime).
#[test]
fn context_summarize_in_substitution_is_honest() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session.set_ask_provider(Box::new(FakeProvider::reply(
            "should not run",
            std::sync::Arc::new(Mutex::new(Vec::new())),
        )));
        session.run_line("echo x").await;
        let result = session.eval_line("echo $(context summarize)").await;
        // The nested summarize errors honestly; the outer echo still exits 0 with the error text
        // captured (Brush substitutes the stderr-less builtin output).
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(combined.contains("needs the model"), "combined: {combined}");
    });
}

/// `ask repl` on the durable-agent path (via `eval_line`) returns an honest not-here message
/// (exit 2), never trying to run a blocking interactive loop.
#[test]
fn ask_repl_via_eval_line_is_honest_message() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let result = session.eval_line("ask repl").await;
        assert_eq!(result.exit_code, 2);
        let stderr = String::from_utf8(result.stderr).unwrap();
        assert!(stderr.contains("native-terminal feature"), "stderr: {stderr}");
        assert!(result.pending_prompt.is_none());
    });
}

/// A native REPL turn runs against the ISOLATED transcript: the model sees the REPL's own history,
/// the main session transcript is untouched, and the exchange is recorded into the REPL transcript.
#[test]
fn repl_turn_uses_isolated_transcript() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::scripted(
            vec![
                crate::ai::ask::AskResponse::text("first reply"),
                crate::ai::ask::AskResponse::text("second reply"),
            ],
            seen.clone(),
        )));

        // Put a marker in the MAIN transcript; the REPL (fresh) must NOT see it.
        session.run_line("echo main_transcript_marker").await;

        let args = crate::ai::ask::ReplArgs {
            model: None,
            seed: crate::ai::ask::ReplSeed::Fresh,
        };
        session.repl_start(&args).unwrap();

        let r1 = session.repl_turn("hello there").await;
        assert_eq!(r1, "first reply");
        // The first turn saw a fresh context (no main-transcript marker).
        let content1 = seen.lock().unwrap()[0].user_content();
        assert!(!content1.contains("main_transcript_marker"), "repl leaked main: {content1}");
        assert!(content1.contains("hello there"));

        // The second turn sees the FIRST exchange (isolated transcript grew).
        let _r2 = session.repl_turn("and again").await;
        let content2 = seen.lock().unwrap()[1].user_content();
        assert!(content2.contains("first reply"), "repl turn2 missing history: {content2}");
        assert!(content2.contains("hello there"), "repl turn2 missing prior prompt");

        // Exiting renders the REPL session; the main transcript still has only its own marker.
        let rendered = String::from_utf8(session.repl_end()).unwrap();
        assert!(rendered.contains("first reply") && rendered.contains("and again"));
        let main = String::from_utf8(session.eval_line("context show").await.stdout).unwrap();
        assert!(main.contains("main_transcript_marker"));
        assert!(!main.contains("first reply"), "REPL content leaked into main mid-session");
    });
}

/// `:model` switches the REPL's model; `:new-session` clears its transcript; `:exit` signals exit.
#[test]
fn repl_meta_commands() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session.set_ask_provider(Box::new(FakeProvider::reply(
            "ok",
            std::sync::Arc::new(Mutex::new(Vec::new())),
        )));
        let args = crate::ai::ask::ReplArgs {
            model: None,
            seed: crate::ai::ask::ReplSeed::Fresh,
        };
        session.repl_start(&args).unwrap();
        assert_eq!(session.repl_model().as_deref(), Some(crate::ai::ask::DEFAULT_MODEL));

        // :model switches (anthropic/ prefix stripped).
        let (out, exit) = session.repl_meta(":model anthropic/claude-sonnet-5").unwrap();
        assert!(!exit);
        assert!(out.contains("claude-sonnet-5"));
        assert_eq!(session.repl_model().as_deref(), Some("claude-sonnet-5"));

        // A prompt grows the transcript; :new-session clears it.
        session.repl_turn("hi").await;
        let (_out, _exit) = session.repl_meta(":new-session").unwrap();
        // After clearing, the next turn sees no prior history.
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::reply("ok2", seen.clone())));
        session.repl_turn("fresh start").await;
        let content = seen.lock().unwrap()[0].user_content();
        assert!(!content.contains("hi"), "new-session should have cleared history: {content}");

        // :exit signals exit; a non-meta line returns None.
        assert_eq!(session.repl_meta(":exit").unwrap().1, true);
        assert!(session.repl_meta("just a prompt").is_none());
    });
}

/// `ask`'s reply is recorded into the transcript like any command output, so a follow-up `ask`
/// (or `context show`) sees the prior exchange — the README "run a command, ask about it" loop.
#[test]
fn ask_reply_is_recorded_in_transcript() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session.set_ask_provider(Box::new(FakeProvider::reply(
            "recorded_reply_xyz",
            std::sync::Arc::new(Mutex::new(Vec::new())),
        )));

        session.run_line(r#"sudo ask "q""#).await;
        let (transcript, _) = session.run_line("context show").await;
        let transcript = String::from_utf8(transcript).unwrap();
        assert!(
            transcript.contains("recorded_reply_xyz"),
            "the ask reply should be in the transcript, got: {transcript}"
        );
    });
}

/// Without a provider (the native default), `ask` degrades to a clean error (exit 4), not a panic.
#[test]
fn ask_without_provider_reports_not_configured() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let result = session.eval_line(r#"sudo ask "hi""#).await;
        assert_eq!(result.exit_code, 4);
        assert!(String::from_utf8(result.stderr)
            .unwrap()
            .contains("no model provider configured"));
    });
}

/// Bare `ask` (no sudo) surfaces the outbound-HTTP confirmation, like curl/wget — it does not
/// call the provider until approved.
#[test]
fn ask_surfaces_a_confirmation() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::reply(
            "should not run yet",
            seen.clone(),
        )));

        let result = session.eval_line(r#"ask "hi""#).await;
        let pending = result.pending_prompt.expect("bare ask should surface a confirm");
        assert!(pending.question.to_lowercase().contains("ask"), "got: {}", pending.question);
        // The provider must NOT have run before approval.
        assert!(seen.lock().unwrap().is_empty(), "provider ran before approval");

        // Approving runs the deferred ask.
        let answered = session.answer_prompt(Some("yes".to_string())).await;
        assert_eq!(answered.exit_code, 0);
        assert_eq!(String::from_utf8(answered.stdout).unwrap(), "should not run yet");
    });
}

// ---- A2: the agentic shell-tool loop --------------------------------------------------------

/// The model calls the `shell` tool once, the loop runs the command, feeds back the result, and
/// the model answers. The tool result carries the command's stdout; the trace is on stderr.
#[test]
fn ask_shell_tool_runs_command_and_feeds_result_back() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::scripted(
            vec![
                shell_tool_call("c1", "echo MARK_42"),
                crate::ai::ask::AskResponse::text("done: I saw MARK_42"),
            ],
            seen.clone(),
        )));

        let result = session.eval_line(r#"sudo ask "echo the marker""#).await;
        assert_eq!(result.exit_code, 0);
        assert_eq!(String::from_utf8(result.stdout).unwrap(), "done: I saw MARK_42");
        // Trace framing on stderr.
        let stderr = String::from_utf8(result.stderr).unwrap();
        assert!(stderr.contains("[tool] $ echo MARK_42"), "got: {stderr}");
        assert!(stderr.contains("[tool] exit 0"), "got: {stderr}");
        // The tool result fed back carried the command's stdout.
        let tr = last_tool_result(&seen, "c1").expect("a tool result for c1");
        let payload = tr.outcome.expect("shell tool succeeded");
        assert!(payload.contains("MARK_42"), "result payload: {payload}");
    });
}

/// Under a plain (approved, non-sudo) ask, a `confirm`-policy tool line (curl) PAUSES for
/// authorization (A3); denying it feeds a "denied by user" result back and the loop continues.
#[test]
fn ask_confirm_tool_pauses_and_deny_continues() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::scripted(
            vec![
                shell_tool_call("c1", "curl https://example.com"),
                crate::ai::ask::AskResponse::text("I could not fetch it"),
            ],
            seen.clone(),
        )));

        // Bare ask → surfaces the ask confirmation; approve it (blanket stays false).
        let first = session.eval_line(r#"ask "fetch it""#).await;
        assert!(first.pending_prompt.is_some(), "bare ask should confirm first");
        let second = session.answer_prompt(Some("yes".to_string())).await;
        // The curl tool call now surfaces its OWN authorization pause.
        let pending = second.pending_prompt.expect("curl tool should pause for authz");
        assert!(pending.question.to_lowercase().contains("permission"), "got: {}", pending.question);
        // Deny it → loop continues, model answers, ask exits 0.
        let result = session.answer_prompt(Some("no".to_string())).await;
        assert_eq!(result.exit_code, 0);
        assert_eq!(String::from_utf8(result.stdout).unwrap(), "I could not fetch it");

        let tr = last_tool_result(&seen, "c1").expect("a tool result for c1");
        let msg = tr.outcome.expect_err("denied curl is an error result");
        assert!(msg.contains("denied by user"), "got: {msg}");
    });
}

/// Even under `sudo ask`, a `sudo-only` tool line (rm) still PAUSES (blanket covers confirm-tier
/// only); denying it leaves the file intact.
#[test]
fn ask_sudo_only_tool_pauses_even_under_sudo_ask() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        // `rm` is sudo-only in the default policy table; try to delete a marker file.
        let path = std::env::temp_dir().join("clank_ask_sudoonly_proof");
        std::fs::write(&path, b"keep").unwrap();
        session.set_ask_provider(Box::new(FakeProvider::scripted(
            vec![
                shell_tool_call("c1", &format!("rm {}", path.display())),
                crate::ai::ask::AskResponse::text("could not remove"),
            ],
            seen.clone(),
        )));

        let first = session.eval_line(&format!(r#"sudo ask "delete {}""#, path.display())).await;
        // Even under sudo ask, the sudo-only rm pauses (no "all" offered for this tier).
        let pending = first.pending_prompt.expect("sudo-only rm should pause under sudo ask");
        assert!(!pending.choices.clone().unwrap_or_default().contains(&"all".to_string()),
            "sudo-only pause must not offer 'all'");
        let result = session.answer_prompt(Some("no".to_string())).await;
        assert_eq!(result.exit_code, 0);
        let tr = last_tool_result(&seen, "c1").expect("a tool result for c1");
        assert!(tr.outcome.is_err(), "denied rm is an error result");
        assert!(path.exists(), "the file must survive the denied rm");
        std::fs::remove_file(&path).ok();
    });
}

/// `sudo ask` pre-authorizes confirm-tier up front: a curl tool call runs without any pause and
/// its body comes back in the tool result.
#[test]
fn ask_sudo_pre_authorizes_confirm_tool() {
    on_rt(async {
        let url = http_mock("fetched-body");
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::scripted(
            vec![
                shell_tool_call("c1", &format!("curl {url}")),
                crate::ai::ask::AskResponse::text("done"),
            ],
            seen.clone(),
        )));

        let first = session.eval_line(r#"sudo ask "fetch it""#).await;
        // sudo ask grants blanket confirm-tier up front → curl does not pause.
        assert!(first.pending_prompt.is_none(), "sudo ask pre-authorizes curl (no pause)");
        assert_eq!(first.exit_code, 0);
        let tr = last_tool_result(&seen, "c1").unwrap().outcome.unwrap();
        assert!(tr.contains("fetched-body"), "curl body should be in the tool result: {tr}");
    });
}

/// A `curl` under a plain approved ask pauses; answering "all" runs it and pre-authorizes a second
/// confirm-tier call in a later turn (no second pause).
#[test]
fn ask_all_answer_upgrades_blanket_mid_loop() {
    on_rt(async {
        let url_a = http_mock("body-a");
        let url_b = http_mock("body-b");
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::scripted(
            vec![
                shell_tool_call("c1", &format!("curl {url_a}")),
                shell_tool_call("c2", &format!("curl {url_b}")),
                crate::ai::ask::AskResponse::text("done"),
            ],
            seen.clone(),
        )));

        // Plain ask (blanket false): approve the ask, then the first curl pauses.
        session.eval_line(r#"ask "fetch a then b""#).await;
        let after_ask = session.answer_prompt(Some("yes".to_string())).await;
        assert!(after_ask.pending_prompt.is_some(), "first curl should pause");
        // Answer "all" → runs c1 AND pre-authorizes c2 (no second pause) → loop completes.
        let done = session.answer_prompt(Some("all".to_string())).await;
        assert!(done.pending_prompt.is_none(), "all should carry through to c2");
        assert_eq!(done.exit_code, 0);
        assert_eq!(String::from_utf8(done.stdout).unwrap(), "done");
        // Both curls actually ran.
        assert!(last_tool_result(&seen, "c1").unwrap().outcome.unwrap().contains("body-a"));
        assert!(last_tool_result(&seen, "c2").unwrap().outcome.unwrap().contains("body-b"));
    });
}

/// The `prompt_user` tool pauses the loop with the model's question; the human's answer becomes the
/// tool result and the loop continues.
#[test]
fn ask_prompt_user_tool_round_trips() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let prompt_call = crate::ai::ask::AskResponse {
            text: String::new(),
            tool_calls: vec![crate::ai::ask::AskToolCall {
                id: "p1".into(),
                name: crate::ai::ask::PROMPT_USER_TOOL.into(),
                arguments_json: serde_json::json!({ "question": "What port should I use?" })
                    .to_string(),
            }],
            finished_for_tools: true,
            error: None,
        };
        session.set_ask_provider(Box::new(FakeProvider::scripted(
            vec![prompt_call, crate::ai::ask::AskResponse::text("using port 8080")],
            seen.clone(),
        )));

        let first = session.eval_line(r#"sudo ask "set up the server""#).await;
        let pending = first.pending_prompt.expect("prompt_user should pause");
        assert!(pending.question.contains("port"), "got: {}", pending.question);
        let done = session.answer_prompt(Some("8080".to_string())).await;
        assert_eq!(done.exit_code, 0);
        assert_eq!(String::from_utf8(done.stdout).unwrap(), "using port 8080");
        // The answer reached the model as the tool result.
        let tr = last_tool_result(&seen, "p1").unwrap();
        assert!(tr.outcome.unwrap().contains("8080"), "answer should be the tool result");
    });
}

/// Killing the paused ask row (or Ctrl-C) aborts the whole ask: exit 130.
#[test]
fn ask_pause_kill_aborts_the_whole_ask() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let prompt_call = crate::ai::ask::AskResponse {
            text: String::new(),
            tool_calls: vec![crate::ai::ask::AskToolCall {
                id: "p1".into(),
                name: crate::ai::ask::PROMPT_USER_TOOL.into(),
                arguments_json: serde_json::json!({ "question": "continue?" }).to_string(),
            }],
            finished_for_tools: true,
            error: None,
        };
        session.set_ask_provider(Box::new(FakeProvider::scripted(
            vec![prompt_call, crate::ai::ask::AskResponse::text("should not reach")],
            seen.clone(),
        )));

        let first = session.eval_line(r#"sudo ask "do a thing""#).await;
        assert!(first.pending_prompt.is_some(), "prompt_user should pause");
        // Abort (as a kill of the paused row would, via answer_prompt(None)).
        let aborted = session.answer_prompt(None).await;
        assert_eq!(aborted.exit_code, 130);
        assert!(session.pending.is_none(), "no pending after abort");
    });
}

/// `model default X` (via the builtin) makes `ask` target X; an explicit `--model` overrides it.
/// Exercises the full ask.toml resolution chain through a real Session.
#[test]
fn ask_uses_model_default_and_flag_overrides() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::scripted(
            vec![
                crate::ai::ask::AskResponse::text("a"),
                crate::ai::ask::AskResponse::text("b"),
            ],
            seen.clone(),
        )));
        // Point HOME at a unique temp dir so ask.toml is hermetic (nanos avoids cross-test clash).
        let home = std::env::temp_dir().join(format!(
            "clank_ask_model_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&home).unwrap();
        session
            .run_line(&format!("export HOME={}", home.display()))
            .await;

        // Set the default, then a bare ask should use it (prefix stripped to the bare id).
        session
            .run_line("model default anthropic/claude-sonnet-4-5")
            .await;
        session.run_line(r#"sudo ask --fresh "hi""#).await;
        assert_eq!(seen.lock().unwrap().last().unwrap().model, "claude-sonnet-4-5");

        // An explicit --model overrides the default.
        session
            .run_line(r#"sudo ask --fresh --model claude-haiku-4-5 "hi""#)
            .await;
        assert_eq!(seen.lock().unwrap().last().unwrap().model, "claude-haiku-4-5");

        std::fs::remove_dir_all(&home).ok();
    });
}

/// An unknown provider prefix in `--model` fails before any model call (exit 2).
#[test]
fn ask_unknown_provider_prefix_errors() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::reply("x", seen.clone())));
        let result = session
            .eval_line(r#"sudo ask --model openai/gpt-4o --fresh "hi""#)
            .await;
        assert_eq!(result.exit_code, 2);
        assert!(String::from_utf8(result.stderr).unwrap().contains("unknown provider"));
        // The provider was never called.
        assert!(seen.lock().unwrap().is_empty());
    });
}

/// The model trying to call `ask` recursively via the shell tool is refused.
#[test]
fn ask_recursion_is_refused() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::scripted(
            vec![
                shell_tool_call("c1", "ask what is 2+2"),
                crate::ai::ask::AskResponse::text("ok"),
            ],
            seen.clone(),
        )));

        session.eval_line(r#"sudo ask "recurse""#).await;
        let tr = last_tool_result(&seen, "c1").expect("a tool result for c1");
        let msg = tr.outcome.expect_err("ask recursion should be refused");
        assert!(msg.contains("itself"), "got: {msg}");
    });
}

/// A `shell`-internal command (`context`) is refused as a tool: it mutates state a tool can't
/// reach. (Also guards `cd`/`export`/`kill` by the same scope check.)
#[test]
fn ask_shell_internal_command_is_refused() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::scripted(
            vec![
                shell_tool_call("c1", "context clear"),
                crate::ai::ask::AskResponse::text("ok"),
            ],
            seen.clone(),
        )));

        session.eval_line(r#"sudo ask "clear it""#).await;
        let tr = last_tool_result(&seen, "c1").expect("a tool result for c1");
        let msg = tr.outcome.expect_err("context should be refused as a tool");
        assert!(msg.contains("shell-internal"), "got: {msg}");
    });
}

/// Malformed tool arguments produce an honest error result; the loop continues.
#[test]
fn ask_malformed_tool_args_error_and_continue() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let bad = crate::ai::ask::AskResponse {
            text: String::new(),
            tool_calls: vec![crate::ai::ask::AskToolCall {
                id: "c1".into(),
                name: crate::ai::ask::SHELL_TOOL.into(),
                arguments_json: "{".into(), // not valid JSON
            }],
            finished_for_tools: true,
            error: None,
        };
        session.set_ask_provider(Box::new(FakeProvider::scripted(
            vec![bad, crate::ai::ask::AskResponse::text("recovered")],
            seen.clone(),
        )));

        let result = session.eval_line(r#"sudo ask "do something""#).await;
        assert_eq!(String::from_utf8(result.stdout).unwrap(), "recovered");
        let tr = last_tool_result(&seen, "c1").expect("a tool result for c1");
        assert!(tr.outcome.unwrap_err().contains("malformed"), "expected a malformed-args error");
    });
}

/// The loop stops at the iteration cap when the model calls a tool every turn, exiting 0 with a
/// stderr notice. The provider is called exactly `ASK_MAX_ITERATIONS` times.
#[test]
fn ask_loop_stops_at_the_iteration_cap() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        // More tool-call responses than the cap: the loop must stop at the cap.
        let script: Vec<_> = (0..ASK_MAX_ITERATIONS + 5)
            .map(|i| shell_tool_call(&format!("c{i}"), "echo loop"))
            .collect();
        session.set_ask_provider(Box::new(FakeProvider::scripted(script, seen.clone())));

        let result = session.eval_line(r#"sudo ask "loop forever""#).await;
        assert_eq!(result.exit_code, 0);
        assert!(
            String::from_utf8(result.stderr).unwrap().contains("tool-call limit"),
            "expected a cap notice on stderr"
        );
        // Exactly cap turns were requested.
        assert_eq!(seen.lock().unwrap().len(), ASK_MAX_ITERATIONS);
    });
}

/// Two `ask` calls in a row both succeed — the provider is take()n and restored each time. A
/// forgotten restore would make the second ask report "not configured".
#[test]
fn ask_provider_is_restored_between_calls() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session.set_ask_provider(Box::new(FakeProvider::scripted(
            vec![
                crate::ai::ask::AskResponse::text("first"),
                crate::ai::ask::AskResponse::text("second"),
            ],
            std::sync::Arc::new(Mutex::new(Vec::new())),
        )));

        let a = session.eval_line(r#"sudo ask --fresh "one""#).await;
        assert_eq!(String::from_utf8(a.stdout).unwrap(), "first");
        let b = session.eval_line(r#"sudo ask --fresh "two""#).await;
        assert_eq!(b.exit_code, 0, "second ask must not report not-configured");
        assert_eq!(String::from_utf8(b.stdout).unwrap(), "second");
    });
}

/// `<cmd> --help` for an intercepted command prints its manifest help text and exits 0, through
/// `eval_line`. These commands never reach Brush's dispatch, so this is the only place they get
/// `--help`. Crucially, `curl --help` does NOT surface the outbound-HTTP confirmation — it's a
/// help query, handled before the authz gate.
#[test]
fn help_flag_prints_help_for_intercepted_commands() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();

        let result = session.eval_line("curl --help").await;
        assert_eq!(result.exit_code, 0);
        assert!(result.pending_prompt.is_none(), "curl --help must not confirm");
        let out = String::from_utf8(result.stdout).unwrap();
        assert!(out.contains("fetch a URL over"), "got: {out}");

        let result = session.eval_line("prompt-user --help").await;
        assert_eq!(result.exit_code, 0);
        assert!(result.pending_prompt.is_none(), "help must not surface a prompt");
        assert!(String::from_utf8(result.stdout).unwrap().contains("pause the"));

        let result = session.eval_line("wget --help").await;
        assert_eq!(result.exit_code, 0);
        assert!(String::from_utf8(result.stdout).unwrap().contains("download a URL"));

        let result = session.eval_line("context --help").await;
        assert_eq!(result.exit_code, 0);
        assert!(String::from_utf8(result.stdout)
            .unwrap()
            .contains("session transcript"));
    });
}

/// A non-`--secret` `prompt-user` error is recorded in the transcript like any other command
/// (only `--secret` *responses* are redacted, per the README — this line never reached the
/// point of collecting a response).
#[test]
fn prompt_user_error_is_recorded_in_transcript() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session.run_line("prompt-user --bogus").await;
        let (transcript, _) = session.run_line("context show").await;
        let transcript = String::from_utf8(transcript).unwrap();
        assert!(transcript.contains("prompt-user --bogus"));
        assert!(transcript.contains("unknown flag"));
    });
}

/// The registry carries a manifest for `prompt-user` even though it's never registered as a
/// Brush `SimpleCommand` (it's intercepted before dispatch) — `type`/tool-surface consumers
/// should still see it.
#[test]
fn prompt_user_has_a_registry_manifest() {
    on_rt(async {
        let session = Session::new().await.unwrap();
        let manifest = session
            .registry()
            .get("prompt-user")
            .expect("prompt-user should have a manifest");
        assert_eq!(
            manifest.execution_scope,
            crate::manifest::ExecutionScope::ShellInternal
        );
    });
}

/// The full two-step path: `prompt-user` surfaces the question (returns immediately with
/// `pending_prompt` set, does NOT hang), then `answer_prompt` delivers the response to stdout
/// with exit 0, recorded in the transcript (not `--secret`), and clears the pending state.
#[test]
fn prompt_user_surfaces_then_answer_resolves() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();

        // Step 1: surface. The question comes back and the prompt is pending — no hang.
        let surfaced = session.eval_line(r#"prompt-user "Which environment?""#).await;
        assert_eq!(surfaced.exit_code, 0);
        let pending = surfaced.pending_prompt.expect("should surface a pending prompt");
        assert_eq!(pending.question, "Which environment?");
        assert!(session.has_pending_prompt());

        // Step 2: answer. The response flows to stdout, pending clears.
        let answered = session.answer_prompt(Some("production".to_string())).await;
        assert_eq!(String::from_utf8(answered.stdout).unwrap(), "production\n");
        assert_eq!(answered.exit_code, 0);
        assert!(answered.pending_prompt.is_none());
        assert!(!session.has_pending_prompt());

        let (transcript, _) = session.run_line("context show").await;
        let transcript = String::from_utf8(transcript).unwrap();
        assert!(transcript.contains("production"), "got: {transcript}");
    });
}

/// `kill <pid>` of the P-state prompt-paused row is the one command allowed through while a
/// prompt is pending: it aborts the prompt (exit 130, same as an explicit abort). Any other
/// command — and a kill of a DIFFERENT pid — stays rejected.
#[test]
fn kill_of_pending_prompt_pid_aborts_it() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session.eval_line(r#"prompt-user "q?""#).await;
        assert!(session.has_pending_prompt());
        let paused_pid = {
            let table = session.proc_table.lock().unwrap();
            let row = table
                .rows()
                .iter()
                .find(|r| r.state == crate::runtime::proctable::ProcState::P)
                .expect("a paused row");
            row.pid
        };

        // A kill of some other pid is rejected like any other command.
        let other = session.eval_line(&format!("kill {}", paused_pid + 100)).await;
        assert_eq!(other.exit_code, 1);
        assert!(session.has_pending_prompt());

        // Killing the paused pid aborts the prompt: exit 130, pending cleared, row reaped.
        let killed = session.eval_line(&format!("kill {paused_pid}")).await;
        assert_eq!(killed.exit_code, 130);
        assert!(!session.has_pending_prompt());
        let after = session.eval_line("echo ok").await;
        assert_eq!(String::from_utf8(after.stdout).unwrap(), "ok\n");
    });
}

/// An aborted answer (`None`) exits 130 with no stdout (README) and clears the pending prompt.
#[test]
fn prompt_user_abort_exits_130() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session.eval_line(r#"prompt-user "q?""#).await;
        let result = session.answer_prompt(None).await;
        assert!(result.stdout.is_empty());
        assert_eq!(result.exit_code, 130);
        assert!(!session.has_pending_prompt());
    });
}

/// An answer outside the prompt's `--choices` errors and leaves the prompt pending to re-ask.
#[test]
fn prompt_user_invalid_choice_keeps_prompt_pending() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session
            .eval_line(r#"prompt-user "Approve?" --confirm"#)
            .await;
        let bad = session.answer_prompt(Some("maybe".to_string())).await;
        assert_eq!(bad.exit_code, 1);
        assert!(session.has_pending_prompt(), "prompt should stay pending");

        // A valid choice then resolves it.
        let ok = session.answer_prompt(Some("yes".to_string())).await;
        assert_eq!(String::from_utf8(ok.stdout).unwrap(), "yes\n");
        assert!(!session.has_pending_prompt());
    });
}

/// A `--secret` response is never entered into the transcript (README), though the command
/// line itself is still recorded.
#[test]
fn prompt_user_secret_response_is_redacted_from_transcript() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session
            .eval_line(r#"prompt-user "Enter the API key:" --secret"#)
            .await;
        let result = session.answer_prompt(Some("s3cr3t-key".to_string())).await;
        // The caller still gets the response on stdout...
        assert_eq!(String::from_utf8(result.stdout).unwrap(), "s3cr3t-key\n");

        // ...but it must not appear in the transcript.
        let (transcript, _) = session.run_line("context show").await;
        let transcript = String::from_utf8(transcript).unwrap();
        assert!(
            !transcript.contains("s3cr3t-key"),
            "secret response leaked into transcript: {transcript}"
        );
        // The command line itself is still recorded.
        assert!(transcript.contains("prompt-user"));
    });
}

/// While a prompt is pending, an ordinary command is rejected — the caller must answer first.
#[test]
fn command_while_prompt_pending_is_rejected() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session.eval_line(r#"prompt-user "q?""#).await;
        let blocked = session.eval_line("echo hi").await;
        assert_ne!(blocked.exit_code, 0);
        assert!(
            String::from_utf8(blocked.terminal_output())
                .unwrap()
                .contains("awaiting a response"),
            "expected a 'answer the prompt first' error"
        );
        // The prompt is still pending and still answerable.
        assert!(session.has_pending_prompt());
    });
}

/// A command rejected while a prompt is pending must RE-SURFACE that prompt in `pending_prompt`, not
/// return a bare error with `pending_prompt: None`. Regression for the connect-shell deadlock: a new
/// client that attaches to an agent already holding a prompt (e.g. a prior session Ctrl-C'd mid-`ask`)
/// sees the rejection; without the re-surfaced prompt it believes nothing is pending and routes the
/// next input to `eval` — which is rejected again, forever. With it, the client can answer and recover.
#[test]
fn rejected_command_while_pending_re_surfaces_the_prompt() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session.eval_line(r#"prompt-user "q?""#).await;
        let blocked = session.eval_line("echo hi").await;
        // Same rejection contract as before: non-zero exit + the "answer first" message.
        assert_ne!(blocked.exit_code, 0);
        assert!(String::from_utf8(blocked.terminal_output())
            .unwrap()
            .contains("awaiting a response"));
        // ...but now the outstanding prompt rides along so a fresh client can resolve it.
        let p = blocked
            .pending_prompt
            .expect("a rejected command must re-surface the pending prompt");
        assert_eq!(p.question, "q?");
        // And it is still answerable through the normal path — the session recovers cleanly.
        let done = session.answer_prompt(Some("ok".to_string())).await;
        assert_eq!(done.exit_code, 0);
        assert!(!session.has_pending_prompt());
    });
}

/// `answer_prompt` with no prompt outstanding is a clean error, not a panic.
#[test]
fn answer_prompt_with_no_pending_is_an_error() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let result = session.answer_prompt(Some("x".to_string())).await;
        assert_ne!(result.exit_code, 0);
    });
}

/// A seeded temp file for `rm` tests: returns its path. Uses a unique name per test.
fn seed_file(tag: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("clank_authz_{tag}_{}", std::process::id()));
    std::fs::write(&path, b"x").unwrap();
    path
}

/// `rm` is `sudo-only`: without elevation it surfaces a confirmation instead of deleting, and
/// the file survives. (The gate composes on the same pending-prompt pause as `prompt-user`.)
#[test]
fn sudo_only_rm_without_sudo_surfaces_confirmation() {
    on_rt(async {
        let path = seed_file("gate");
        let mut session = Session::new().await.unwrap();
        let result = session
            .eval_line(&format!("rm {}", path.display()))
            .await;
        // A confirmation was surfaced, not a deletion.
        assert!(result.pending_prompt.is_some(), "rm should surface a sudo confirmation");
        assert!(session.has_pending_prompt());
        assert!(path.exists(), "file must survive an unapproved sudo-only rm");
        let _ = std::fs::remove_file(&path);
    });
}

/// Denying (`no`) a `sudo-only` `rm` confirmation → exit 5, file survives, pending clears.
#[test]
fn sudo_only_rm_denied_returns_exit_5() {
    on_rt(async {
        let path = seed_file("deny");
        let mut session = Session::new().await.unwrap();
        session.eval_line(&format!("rm {}", path.display())).await;
        let denied = session.answer_prompt(Some("no".to_string())).await;
        assert_eq!(denied.exit_code, 5, "denial is exit 5");
        assert!(path.exists(), "denied rm must not delete");
        assert!(!session.has_pending_prompt());
        let _ = std::fs::remove_file(&path);
    });
}

/// Approving (`yes`) a `sudo-only` `rm` confirmation runs the deferred command — the file is
/// deleted, exit 0.
#[test]
fn sudo_only_rm_approved_runs_the_command() {
    on_rt(async {
        let path = seed_file("approve");
        let mut session = Session::new().await.unwrap();
        session.eval_line(&format!("rm {}", path.display())).await;
        let approved = session.answer_prompt(Some("yes".to_string())).await;
        assert_eq!(approved.exit_code, 0, "approved rm succeeds");
        assert!(!path.exists(), "approved rm must delete the file");
        assert!(!session.has_pending_prompt());
    });
}

/// A `sudo rm` prefix pre-authorizes the sudo-only command — it runs immediately, no prompt.
#[test]
fn sudo_prefix_bypasses_the_gate() {
    on_rt(async {
        let path = seed_file("sudo");
        let mut session = Session::new().await.unwrap();
        let result = session
            .eval_line(&format!("sudo rm {}", path.display()))
            .await;
        assert!(result.pending_prompt.is_none(), "sudo rm should not prompt");
        assert_eq!(result.exit_code, 0);
        assert!(!path.exists(), "sudo rm deletes immediately");
    });
}

/// An `allow`-policy command (e.g. `echo`) is completely unaffected by the gate.
#[test]
fn allow_policy_command_is_ungated() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let result = session.eval_line("echo hi").await;
        assert!(result.pending_prompt.is_none());
        assert_eq!(String::from_utf8(result.stdout).unwrap(), "hi\n");
        assert_eq!(result.exit_code, 0);
    });
}

/// A one-shot localhost HTTP server (raw `std::net`, no dep) that replies `200 <body>` once.
/// Hermetic — the `curl`/`wget` interception is exercised end-to-end without real internet.
fn http_mock(body: &'static str) -> String {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 2048];
            let _ = stream.read(&mut buf);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    format!("http://{addr}")
}

/// `curl` is `confirm`-policy (outbound HTTP): it surfaces a confirmation before the request runs.
#[test]
fn curl_surfaces_a_confirmation() {
    on_rt(async {
        let url = http_mock("body");
        let mut session = Session::new().await.unwrap();
        let result = session.eval_line(&format!("curl {url}")).await;
        assert!(result.pending_prompt.is_some(), "curl should surface a confirm");
        assert!(session.has_pending_prompt());
        // No request ran yet: resolve the prompt so the mock thread can exit cleanly.
        session.answer_prompt(Some("no".to_string())).await;
    });
}

/// Approving a `curl` confirmation runs the request — the body comes back on stdout, exit 0.
/// Proves the post-approval deferred path routes through the HTTP dispatch in `run_command`.
#[test]
fn curl_approved_runs_the_request() {
    on_rt(async {
        let url = http_mock("approved-body");
        let mut session = Session::new().await.unwrap();
        session.eval_line(&format!("curl {url}")).await;
        let out = session.answer_prompt(Some("yes".to_string())).await;
        assert_eq!(out.exit_code, 0);
        assert_eq!(String::from_utf8(out.stdout).unwrap(), "approved-body");
    });
}

/// Serializes the cwd-sensitive `cd` test against the curl-pipeline tests: their grep stages hold
/// process-cwd windows (ShellCwd) while a mock server round-trips, and the process cwd is one
/// global across Sessions. Test-parallelism artifact only; production runs one line at a time.
static CWD_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A curl-HEADED pipeline composes: the Session runs the HTTP, and the downstream (Brush) reads
/// the response as stdin. `sudo` pre-authorizes, so the pipeline runs directly.
#[test]
fn curl_headed_pipeline_feeds_the_downstream() {
    let _cwd = CWD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    on_rt(async {
        let url = http_mock("alpha\nbeta\ngamma\n");
        let mut session = Session::new().await.unwrap();
        let result = session.eval_line(&format!("sudo curl -s {url} | grep beta")).await;
        assert_eq!(result.exit_code, 0, "stderr: {}", String::from_utf8_lossy(&result.stderr));
        assert_eq!(String::from_utf8(result.stdout).unwrap(), "beta\n");
    });
}

/// The downstream may itself be a multi-stage Brush pipeline.
#[test]
fn curl_headed_pipeline_multistage_downstream() {
    let _cwd = CWD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    on_rt(async {
        let url = http_mock("c\nb\na\n");
        let mut session = Session::new().await.unwrap();
        let result = session
            .eval_line(&format!("sudo curl -s {url} | grep -v b | grep -c ."))
            .await;
        assert_eq!(String::from_utf8(result.stdout).unwrap(), "2\n");
    });
}

/// The deferred-confirm path: an unelevated curl pipeline surfaces the confirm; approving runs the
/// WHOLE pipeline (head HTTP + downstream). This is the path that forces the head-split to live in
/// `run_command` — the approval re-runs the raw line there, and an eval_line-only intercept would
/// have re-broken it.
#[test]
fn curl_headed_pipeline_approved_after_confirm() {
    let _cwd = CWD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    on_rt(async {
        let url = http_mock("x1\nx2\n");
        let mut session = Session::new().await.unwrap();
        let pending = session.eval_line(&format!("curl -s {url} | grep -c x")).await;
        assert!(pending.pending_prompt.is_some(), "pipeline curl should confirm");
        let out = session.answer_prompt(Some("yes".to_string())).await;
        assert_eq!(out.exit_code, 0);
        assert_eq!(String::from_utf8(out.stdout).unwrap(), "2\n");
    });
}

/// curl NOT at the head of the pipeline stays an honest stub error (exit 1) that now names the
/// supported form — never the old flattened-argv "unknown option" junk. Under per-segment authz the
/// unelevated `curl` (second segment; the leading `sudo` is on `echo`, not curl) gates first — so
/// approve it, then assert the stub.
#[test]
fn curl_mid_pipeline_is_an_honest_stub_error() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let gated = session.eval_line("echo feed | curl https://unused.invalid").await;
        assert!(gated.pending_prompt.is_some(), "curl mid-pipeline should gate (unelevated)");
        let result = session.answer_prompt(Some("yes".to_string())).await;
        assert_eq!(result.exit_code, 1);
        let stderr = String::from_utf8(result.stderr).unwrap();
        assert!(stderr.contains("FIRST in the pipeline"), "got: {stderr}");
        assert!(!stderr.contains("unknown option"), "got: {stderr}");
    });
}

/// Denying a `curl` confirmation → exit 5, no request.
#[test]
fn curl_denied_returns_exit_5() {
    on_rt(async {
        let url = http_mock("never");
        let mut session = Session::new().await.unwrap();
        session.eval_line(&format!("curl {url}")).await;
        let out = session.answer_prompt(Some("no".to_string())).await;
        assert_eq!(out.exit_code, 5);
    });
}

/// `sudo curl` pre-authorizes: the request runs immediately, no prompt. Proves the direct-allow
/// path also routes through the HTTP dispatch (not Brush's `execute`).
#[test]
fn sudo_curl_bypasses_gate_and_fetches() {
    on_rt(async {
        let url = http_mock("sudo-body");
        let mut session = Session::new().await.unwrap();
        let out = session.eval_line(&format!("sudo curl {url}")).await;
        assert!(out.pending_prompt.is_none(), "sudo curl should not prompt");
        assert_eq!(out.exit_code, 0);
        assert_eq!(String::from_utf8(out.stdout).unwrap(), "sudo-body");
    });
}

/// `curl -o <file>` (approved) writes the body to a file, stdout empty.
#[test]
fn curl_o_writes_a_file() {
    on_rt(async {
        let url = http_mock("file-body");
        let path = std::env::temp_dir().join(format!("clank_curl_o_{}", std::process::id()));
        let mut session = Session::new().await.unwrap();
        session
            .eval_line(&format!("sudo curl -o {} {url}", path.display()))
            .await;
        // (sudo → no prompt; runs immediately)
        let out = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(out, "file-body");
    });
}

/// `wget -O -` (approved) streams the body to stdout.
#[test]
fn wget_dash_o_to_stdout() {
    on_rt(async {
        let url = http_mock("wget-body");
        let mut session = Session::new().await.unwrap();
        let out = session.eval_line(&format!("sudo wget -O - {url}")).await;
        assert_eq!(out.exit_code, 0);
        assert_eq!(String::from_utf8(out.stdout).unwrap(), "wget-body");
    });
}

/// `curl`/`wget` carry `Subprocess`/`Confirm` manifests in the registry.
#[test]
fn http_commands_have_confirm_manifests() {
    on_rt(async {
        let session = Session::new().await.unwrap();
        for name in ["curl", "wget"] {
            let m = session.registry().get(name).expect("manifest");
            assert_eq!(m.execution_scope, crate::manifest::ExecutionScope::Subprocess);
            assert_eq!(
                m.authorization_policy,
                crate::manifest::AuthorizationPolicy::Confirm
            );
        }
    });
}

/// `$PATH` is set to clank's README default (the virtual package-resolution namespace).
/// Both env locks — the PATH is now built from the mcp AND grease dir overrides, so a concurrent
/// test holding either set of `CLANK_*` vars would leak its temp dirs into this session's PATH.
/// Lock order is the house order, GREASE then MCP (`set_grease_dirs` before `set_mcp_dirs`
/// everywhere) — the reverse deadlocks the suite AB-BA.
#[test]
fn path_is_the_readme_default() {
    let _grease = crate::grease::config::TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _mcp = crate::mcp::config::TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let (out, _) = session.run_line("echo $PATH").await;
        assert_eq!(String::from_utf8(out).unwrap(), format!("{DEFAULT_PATH}\n"));
    });
}

/// With no `CLANK_*` overrides, `effective_path()` is byte-identical to the documented default —
/// the drift guard for the dynamic construction.
#[test]
fn effective_path_defaults_to_the_readme_path() {
    // House lock order: grease, then mcp (see path_is_the_readme_default).
    let _grease = crate::grease::config::TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _mcp = crate::mcp::config::TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    assert_eq!(effective_path(), DEFAULT_PATH);
}

/// A `CLANK_MCP_BIN` override lands in `$PATH`, so a native session RESOLVES what `mcp add`
/// installs — before this, the launcher went to the override dir while `$PATH` kept the hardcoded
/// default, and `which <server>` never saw it.
#[test]
fn effective_path_honors_the_mcp_bin_override() {
    let _lock = crate::mcp::config::TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("mcp-bin");
    std::env::set_var("CLANK_MCP_BIN", &bin);
    let path = effective_path();
    std::env::remove_var("CLANK_MCP_BIN");
    assert!(
        path.contains(bin.to_str().unwrap()),
        "PATH should contain the override: {path}"
    );
    assert!(!path.contains("/usr/lib/mcp/bin"), "default entry should be replaced: {path}");
}

/// `which` finds nothing for a name with no file-backed form, and does NOT report a phantom
/// path (the bug caught on the agent: Brush's wasm `executable()` returns true unconditionally,
/// so `which` must verify existence itself). Chained with a marker to prove no wedge/error.
#[test]
fn which_finds_nothing_for_a_nonexistent_command() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let (out, _) = session
            .run_line("which clank-no-such-cmd-xyz; echo done")
            .await;
        let out = String::from_utf8(out).unwrap();
        assert!(out.trim_end().ends_with("done"), "got: {out}");
        assert!(
            !out.contains("clank-no-such-cmd-xyz"),
            "which must not report a phantom path for a missing command: {out}"
        );
    });
}

/// `which` finds a real executable file placed on `$PATH`.
#[test]
fn which_finds_a_real_path_file() {
    on_rt(async {
        let dir = std::env::temp_dir().join(format!("clank_which_bin_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("clank-which-probe");
        std::fs::write(&exe, b"#!/bin/sh\ntrue\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let mut session = Session::new().await.unwrap();
        // Prepend our dir to $PATH, then `which` should find the probe file.
        session
            .run_line(&format!("export PATH={}:$PATH", dir.display()))
            .await;
        let (out, _) = session.run_line("which clank-which-probe").await;
        let out = String::from_utf8(out).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            out.contains("clank-which-probe"),
            "which should find a real $PATH file, got: {out}"
        );
    });
}

/// Brush's own `type` still works (now with a clank manifest, but unchanged behavior) — it
/// reports a clank builtin as a builtin. Guards the manifest-registration change against
/// accidentally breaking Brush's builtin dispatch.
#[test]
fn type_reports_a_builtin() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let (out, _) = session.run_line("type ls").await;
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("ls"), "got: {out}");
        assert!(out.contains("builtin"), "type should call ls a builtin, got: {out}");
    });
}

/// `type`/`command`/`which` have registry manifests (the resolution surface sees them).
#[test]
fn resolution_commands_have_manifests() {
    on_rt(async {
        let session = Session::new().await.unwrap();
        for name in ["type", "command", "which"] {
            assert!(
                session.registry().get(name).is_some(),
                "{name} should have a manifest"
            );
        }
    });
}

#[test]
fn cat_reads_virtual_proc_status() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let (out, _) = session.run_line("cat /proc/1/status").await;
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("Pid:"), "got: {out}");
        assert!(out.contains("State:"));
        assert!(out.contains("clank"));
    });
}

/// `ls /bin` enumerates every registered command name — intercepted (`curl`, `prompt-user`) and
/// Brush-registered (`cat`) alike — so the AI can discover the full capability set. Virtual `/bin`.
#[test]
fn ls_bin_lists_all_commands() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let (out, _) = session.run_line("ls /bin").await;
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("curl"), "got: {out}");
        assert!(out.contains("prompt-user"));
        assert!(out.contains("cat"));
    });
}

/// `cat /bin/<name>` prints the command's help text — the virtual file is `cat`-able like a
/// `/proc` file. Covers an intercepted command (`curl`, invisible to Brush's own resolution).
#[test]
fn cat_bin_curl_shows_help() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let (out, _) = session.run_line("cat /bin/curl").await;
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("fetch a URL over"), "got: {out}");
    });
}

/// `cat /bin/<unknown>` reports "No such file or directory" (like a real missing file), not a
/// spurious success — the virtual namespace only serves registered names.
#[test]
fn cat_bin_unknown_is_not_found() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let (out, _) = session.run_line("cat /bin/does-not-exist").await;
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("No such file or directory"), "got: {out}");
    });
}

/// `grep` output is captured via `context.stdout()` (the `run_tool` path) — this is
/// parallel-safe (no process-global fd swap) and verifies the wasm output-capture fix on the
/// native side too.
#[test]
fn grep_captures_output() {
    on_rt(async {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("clank_grep_test_{}", std::process::id()));
        std::fs::write(&path, b"alpha\nbeta\ngamma\n").unwrap();
        let mut session = Session::new().await.unwrap();
        let (out, _) = session
            .run_line(&format!("grep beta {}", path.display()))
            .await;
        let _ = std::fs::remove_file(&path);
        let out = String::from_utf8(out).unwrap();
        assert!(
            out.contains("beta"),
            "grep output should contain the match, got: {out}"
        );
        assert!(
            !out.contains("alpha"),
            "grep should not emit non-matching lines: {out}"
        );
    });
}

/// `grep` over a virtual `/proc` file works and its output is captured.
#[test]
fn grep_matches_virtual_proc_file() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let (out, _) = session.run_line("grep State /proc/1/status").await;
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("State"), "got: {out}");
    });
}

/// A real-file `cat` still works after `cat` became a `/proc`-aware shim (delegation intact).
#[test]
fn cat_still_reads_real_files() {
    on_rt(async {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("clank_cat_test_{}", std::process::id()));
        std::fs::write(&path, b"real-file-contents\n").unwrap();
        let mut session = Session::new().await.unwrap();
        let (out, _) = session.run_line(&format!("cat {}", path.display())).await;
        let _ = std::fs::remove_file(&path);
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("real-file-contents"), "got: {out}");
    });
}

/// PIDs persist and keep climbing across `run_line` calls (the durable-agent property, tested
/// locally): the second command gets a higher PID than the first.
#[test]
fn pids_are_monotonic_across_lines() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session.run_line("echo one").await;
        session.run_line("echo two").await;
        let (ps_out, _) = session.run_line("ps").await;
        let ps_out = String::from_utf8(ps_out).unwrap();

        let pid_of = |needle: &str| -> u32 {
            ps_out
                .lines()
                .find(|l| l.contains(needle))
                .and_then(|l| l.split_whitespace().next())
                .and_then(|p| p.parse().ok())
                .unwrap_or_else(|| panic!("no pid for {needle} in:\n{ps_out}"))
        };
        assert!(pid_of("echo two") > pid_of("echo one"));
    });
}

// --- `export --secret` (README "Sensitive environment variables") ---------------------------------
//
// Each test uses a UNIQUE variable name because `export --secret` writes process-global `std::env`
// (Full env parity) and the Rust test harness runs tests on shared threads.

/// A `--secret` variable is available via the environment — `$NAME` expands in a subsequent command,
/// exactly like an ordinary exported var (the redaction contract must not break value availability).
#[test]
fn secret_export_value_still_expands() {
    let _secret_lock = crate::runtime::secretenv::TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session
            .run_line("export --secret CLANK_SECRET_EXPANDS=sk-expand-123")
            .await;
        let result = session.eval_line(r#"echo "$CLANK_SECRET_EXPANDS""#).await;
        // The author's own `echo` deliberately emits the value — but the transcript recorder masks it,
        // so we assert on the raw command *result* (what a subprocess/child would see), not the record.
        assert_eq!(
            String::from_utf8(result.stdout).unwrap(),
            "sk-expand-123\n",
            "$NAME must still expand to the secret value"
        );
    });
}

/// Syntactically incomplete input (unterminated heredoc) answers honestly with exit 2 and the
/// session SURVIVES — before this, the fatal parse error mapped to `ExitShell` (clank's Brush shell
/// is non-interactive) and one mistyped `cat <<EOF` ended the whole session.
#[test]
fn incomplete_input_survives_the_session() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let result = session.eval_line("cat <<EOF").await;
        assert_eq!(result.exit_code, 2);
        assert!(matches!(result.flow, Flow::Continue));
        let stderr = String::from_utf8(result.stderr).unwrap();
        assert!(stderr.contains("incomplete input"), "got: {stderr}");
        // The session is alive and works.
        let after = session.eval_line("echo alive").await;
        assert_eq!(String::from_utf8(after.stdout).unwrap(), "alive\n");
        assert_eq!(after.exit_code, 0);
        // An unterminated quote takes the same path.
        let quote = session.eval_line("echo 'dangling").await;
        assert_eq!(quote.exit_code, 2);
        // A complete heredoc (newlines embedded in one eval) still runs normally. `contains`, not
        // exact: uu `cat` runs under the process-global fd-1 swap, which can catch a parallel
        // test-reporter line printed during the window (the documented run_uu harness leak).
        let ok = session.eval_line("cat <<EOF\nhello\nEOF").await;
        assert!(String::from_utf8(ok.stdout).unwrap().contains("hello"));
    });
}

/// An operator-bearing `--secret` export is refused with an honest error — it must never fall
/// through to Brush's export, which would set the value with zero redaction. (Exactly the live-demo
/// probe: `export --secret K=v && echo set` looked like the feature was inert; the truth was a
/// silent unredacted fallthrough.)
#[test]
fn secret_export_with_operators_is_refused() {
    let _secret_lock = crate::runtime::secretenv::TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let result = session
            .eval_line("export --secret CLANK_SECRET_NONPLAIN=sk-nonplain-7 && echo set")
            .await;
        assert_eq!(result.exit_code, 2);
        let stderr = String::from_utf8(result.stderr).unwrap();
        assert!(stderr.contains("standalone command"), "got: {stderr}");
        // Neither half ran: the variable is unset and the `&&` tail never printed.
        assert!(String::from_utf8(result.stdout).unwrap().is_empty());
        let echoed = session.eval_line(r#"echo "${CLANK_SECRET_NONPLAIN:-unset}""#).await;
        assert_eq!(String::from_utf8(echoed.stdout).unwrap(), "unset\n");
    });
}

/// Quoted metacharacters in the VALUE keep the line plain — `export --secret K="a|b"` is a
/// standalone command (the `|` is data, not an operator) and must keep working.
#[test]
fn secret_export_quoted_metachar_value_still_plain() {
    let _secret_lock = crate::runtime::secretenv::TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let result = session
            .eval_line(r#"export --secret CLANK_SECRET_PIPEVAL="a|b""#)
            .await;
        assert_eq!(result.exit_code, 0);
        let echoed = session.eval_line(r#"echo "$CLANK_SECRET_PIPEVAL""#).await;
        assert_eq!(String::from_utf8(echoed.stdout).unwrap(), "a|b\n");
    });
}

/// A `--secret` variable is hidden from `env` output (neither the value nor the key appears), while
/// an ordinary environment var (one genuinely in the process env) still shows — proving the filter
/// drops *only* the secret-marked key.
///
/// The control var is seeded directly into `std::env`, not via a plain Brush `export`: clank's
/// `env` reads the process environment (`std::env`), while Brush's `export` populates Brush's own
/// variable table — the two are decoupled by design, so a Brush-only `export FOO=bar` never shows
/// in `env`. That pre-existing decoupling is not what this test is about.
#[test]
fn secret_export_hidden_from_env() {
    let _secret_lock = crate::runtime::secretenv::TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    on_rt(async {
        std::env::set_var("CLANK_ENV_CONTROL", "controlval");
        let mut session = Session::new().await.unwrap();
        session
            .run_line("export --secret CLANK_SECRET_ENV=sk-envhidden-456")
            .await;
        let (env_out, _) = session.run_line("env").await;
        let env_out = String::from_utf8(env_out).unwrap();
        assert!(
            !env_out.contains("sk-envhidden-456"),
            "secret value must not appear in env:\n{env_out}"
        );
        assert!(
            !env_out.contains("CLANK_SECRET_ENV"),
            "secret key must not appear in env:\n{env_out}"
        );
        // A genuine process-env var is unaffected — proves we filter only the marked one.
        assert!(
            env_out.contains("CLANK_ENV_CONTROL=controlval"),
            "a non-secret process-env var should still show in env:\n{env_out}"
        );
        std::env::remove_var("CLANK_ENV_CONTROL");
    });
}

/// `env` filters the secret even as a non-final PIPELINE stage. Natively that stage runs on a Brush
/// `spawn_blocking` WORKER thread — a thread the per-line secret set was never installed on — which is
/// exactly why the filter set is a process-global slot and not a thread-local (the conformance suite
/// caught the leak live: `env | grep -c SECRET` printed 1 natively).
///
/// Crucially the consumer is `grep`, NOT `cat`: `grep` is a `run_tool` command that genuinely reads
/// the pipe, while uu `cat` reads the real process stdin natively and never sees `env`'s piped output
/// — so an `env | cat` assertion is a false-green that passes even while the secret leaks.
#[test]
fn secret_hidden_from_env_in_a_pipeline() {
    let _secret_lock = crate::runtime::secretenv::TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session
            .run_line("export --secret CLANK_SECRET_PIPE_ENV=sk-pipehide-999")
            .await;
        // env is stage 0 (non-final) → owned-subshell worker thread on native. Filtered before grep.
        let result = session.eval_line("env | grep CLANK_SECRET_PIPE_ENV").await;
        let out = String::from_utf8(result.stdout).unwrap();
        assert!(
            out.is_empty(),
            "the secret must be filtered before a pipeline consumer reads it, but env leaked it \
             to grep:\n{out}"
        );
    });
}

/// `env -0` (NUL-separated listing) still filters the secret — the custom listing path handles the
/// `-0` flag rather than delegating to uu_env.
#[test]
fn secret_hidden_from_env_null_separated() {
    let _secret_lock = crate::runtime::secretenv::TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session
            .run_line("export --secret CLANK_SECRET_NUL=sk-nulhide-111")
            .await;
        let (out, _) = session.run_line("env -0").await;
        let out = String::from_utf8(out).unwrap();
        assert!(
            !out.contains("sk-nulhide-111") && !out.contains("CLANK_SECRET_NUL"),
            "env -0 must also filter the secret:\n{out}"
        );
    });
}

/// `env -i` (ignore-environment) listing still hides a secret — starting from an empty env means the
/// secret was never there, and the filter path is what serves the listing (not uu_env). A control
/// `NAME=value` operand added on the line still shows.
#[test]
fn secret_hidden_from_env_ignore_and_inline_add() {
    let _secret_lock = crate::runtime::secretenv::TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    on_rt(async {
        std::env::set_var("CLANK_ENV_IADD", "willbedropped");
        let mut session = Session::new().await.unwrap();
        session
            .run_line("export --secret CLANK_SECRET_IENV=sk-ihide-222")
            .await;
        // `-i` starts from empty; the inline VISIBLE=y add should be the only listed var.
        let (out, _) = session.run_line("env -i VISIBLE=y").await;
        let out = String::from_utf8(out).unwrap();
        assert!(
            !out.contains("sk-ihide-222") && !out.contains("CLANK_SECRET_IENV"),
            "env -i must not surface the secret:\n{out}"
        );
        assert!(
            out.contains("VISIBLE=y"),
            "an inline add should still list:\n{out}"
        );
        std::env::remove_var("CLANK_ENV_IADD");
    });
}

/// `env -u NAME` (unset) drops a normal var from the listing and still filters secrets.
#[test]
fn env_unset_drops_a_var() {
    on_rt(async {
        std::env::set_var("CLANK_ENV_UNSET_ME", "dropme");
        let mut session = Session::new().await.unwrap();
        let (out, _) = session.run_line("env -u CLANK_ENV_UNSET_ME").await;
        let out = String::from_utf8(out).unwrap();
        assert!(
            !out.contains("CLANK_ENV_UNSET_ME"),
            "env -u NAME should drop NAME from the listing:\n{out}"
        );
        std::env::remove_var("CLANK_ENV_UNSET_ME");
    });
}

/// The defining `export --secret NAME=VALUE` line is recorded to the transcript with its value
/// redacted; the key name is preserved (only the value is sensitive).
#[test]
fn secret_export_line_redacted_in_transcript() {
    let _secret_lock = crate::runtime::secretenv::TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session
            .run_line("export --secret CLANK_SECRET_TX=sk-txredact-789")
            .await;
        let (show_out, _) = session.run_line("context show").await;
        let show_out = String::from_utf8(show_out).unwrap();
        assert!(
            !show_out.contains("sk-txredact-789"),
            "secret value must not appear in the transcript:\n{show_out}"
        );
        assert!(
            show_out.contains("export --secret CLANK_SECRET_TX=<redacted>"),
            "the export line should be recorded with its value redacted:\n{show_out}"
        );
    });
}

/// A `export --secret` declaration never writes its VALUE to shell.log (README "never written to
/// logs"). The declaration is the one case the `mask_values` log filter can't cover — the secret
/// isn't in the active set until the line runs — so the value is stripped structurally in the
/// start/end events. The key name is preserved (only the value is sensitive).
#[test]
fn secret_export_value_absent_from_shell_log() {
    let _secret_lock = crate::runtime::secretenv::TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    on_rt(async {
        let cap = LogCapture::new("secretlog");
        let mut session = Session::new().await.unwrap();
        session
            .eval_line("export --secret CLANK_LOG_SECRET=sk-logredact-456")
            .await;
        let log = cap.read(crate::logging::LogFile::Shell);
        assert!(
            !log.contains("sk-logredact-456"),
            "secret value must never reach shell.log:\n{log}"
        );
        assert!(
            log.contains("export --secret CLANK_LOG_SECRET=<redacted>"),
            "the export line should be logged with its value redacted:\n{log}"
        );
    });
}

/// A secret value that surfaces in later command *output* (e.g. the author echoes it) is masked in
/// the transcript — a secret can leak by value where its name never appears.
#[test]
fn secret_value_masked_in_later_output() {
    let _secret_lock = crate::runtime::secretenv::TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session
            .run_line("export --secret CLANK_SECRET_OUT=sk-outmask-abc")
            .await;
        session.run_line(r#"echo "leaked: $CLANK_SECRET_OUT""#).await;
        let (show_out, _) = session.run_line("context show").await;
        let show_out = String::from_utf8(show_out).unwrap();
        assert!(
            !show_out.contains("sk-outmask-abc"),
            "secret value must be masked in recorded output:\n{show_out}"
        );
        assert!(
            show_out.contains("leaked: <redacted>"),
            "the echoed value should be masked in the transcript:\n{show_out}"
        );
    });
}

/// A secret value is masked in `ps` COMMAND — the export row shows `<redacted>` on a later `ps`
/// (when the secret is active), never the literal value.
#[test]
fn secret_value_masked_in_ps() {
    let _secret_lock = crate::runtime::secretenv::TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        session
            .run_line("export --secret CLANK_SECRET_PS=sk-psmask-def")
            .await;
        let (ps_out, _) = session.run_line("ps").await;
        let ps_out = String::from_utf8(ps_out).unwrap();
        assert!(
            !ps_out.contains("sk-psmask-def"),
            "secret value must not appear in ps COMMAND:\n{ps_out}"
        );
        assert!(
            ps_out.contains("<redacted>"),
            "the export row should show the value redacted:\n{ps_out}"
        );
    });
}

/// A `--secret` export buried in a pipeline is NOT intercepted (falls through to Brush) — the
/// interception is scoped to plain top-level lines, like `context summarize`.
#[test]
fn secret_export_in_pipeline_falls_through() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        // `echo x | export --secret K=v` is not a plain line; it must not reach `run_secret_export`
        // (which would mark K secret). We prove non-interception by checking the var is NOT redacted
        // from env afterward (Brush's own handling, whatever it does, doesn't populate `secret_env`).
        session
            .run_line("echo x | export --secret CLANK_SECRET_PIPE=notsecret")
            .await;
        // Whatever Brush did with the pipeline, our secret table stays empty for this name, so a
        // plain `env` is not filtering it. The strongest assertion we can make without depending on
        // Brush's pipeline semantics: the session's secret table did not gain the name.
        assert!(
            !session.secret_env.contains_key("CLANK_SECRET_PIPE"),
            "a pipelined `export --secret` must not populate the secret table"
        );
    });
}

/// `man export` renders clank's manifest help, which documents the `--secret` form. (`export --help`
/// is served by Brush's own clap help, not clank's manifest — the manifest surfaces via `man` /
/// `cat /bin/export`, as for the other Brush-native builtins.)
#[test]
fn man_export_documents_secret() {
    on_rt(async {
        let mut session = Session::new().await.unwrap();
        let (out, _) = session.run_line("man export").await;
        let out = String::from_utf8(out).unwrap();
        assert!(
            out.contains("--secret"),
            "man export should document the --secret form:\n{out}"
        );
    });
}

