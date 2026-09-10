// The session test suite (included via `#[cfg(all(test, not(wasm)))] mod tests` in mod.rs).
// unwrap/expect on known-good fixtures is correct test style; clippy's allow-unwrap-in-tests does not
// recognize the compound cfg gate, so scope it explicitly here — it covers the submodules too.
#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Shared test harness, plus one submodule per concern.
//!
//! Everything in THIS file is fixture: the fake `AskProvider`/`McpHttp`/`AgentInvoker`
//! implementations, the env-var guards that make grease and MCP hermetic, and the small builders
//! that shape a canned HTTP response. The submodules hold the assertions, and reach the fixtures
//! through `use super::*`.
//!
//! The split mirrors `session/*.rs` where a concern has its own module there, and adds a file where
//! a concern is spread across several (`secrets`, `authz`, `resolution`, `logging`).
//!
//! ## Why submodules of one `mod tests`, and not files under `tests/` at the crate root
//!
//! This matters more than it looks, and an earlier attempt at this split was abandoned over test
//! races that turned out to be a consequence of getting it wrong.
//!
//! Crate-root integration tests compile to **separate binaries**, so each runs in its own process.
//! Several things this suite depends on are **process-global**, not per-test:
//!
//! - the process working directory, which `tools::coreutils::ShellCwd` moves for the duration of a
//!   builtin call (brush keeps `cd` in its own state and never touches the process cwd);
//! - `$CLANK_GREASE_*`, `$CLANK_MCP_*` and `$CLANK_LOG_DIR`, which the hermetic-dir guards set and
//!   restore;
//! - uucore's exit code, an `AtomicI32` upstream only ever resets at process exit;
//! - the `SIGPIPE` disposition that `run_uu` flips around a `uumain` call.
//!
//! The locks that serialize all of that — [`CWD_TEST_LOCK`], `grease::config::TEST_ENV_LOCK`,
//! `mcp::config::TEST_ENV_LOCK`, `logging::test_env_lock` — are `static`s, so they serialize
//! **within one process and not across processes**. Splitting into separate binaries silently
//! removes every one of those guarantees while leaving the code that assumes them untouched, which
//! is exactly the shape that produces intermittent, load-dependent failures.
//!
//! Submodules keep one binary, one process, one set of locks. The tests are unchanged and so are
//! their guarantees — verified by the count being identical across the split (509) and by 12
//! consecutive clean runs.
//!
//! Note the lock ORDER convention, which the split preserves: **grease, then mcp**. Taking them in
//! the other order deadlocks against a test that takes them in this one.

use super::*;

mod agent;
mod ask;
mod authz;
mod eval;
mod grease;
mod http;
mod logging;
mod mcp;
mod prompt;
mod resolution;
mod secrets;

/// Drive a closure on a fresh current-thread runtime (mirrors how `Session` is used natively).
fn on_rt<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(f)
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
        Self::scripted(vec![crate::ai::ask::AskResponse::text(reply)], seen)
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
            name: crate::ai::prompts::SHELL_TOOL.to_string(),
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
    ) -> crate::mcp::error::Result<crate::mcp::client::HttpResponse> {
        let method_and_body = format!(
            "{method} {}",
            String::from_utf8_lossy(&body.unwrap_or_default())
        );
        self.seen
            .lock()
            .unwrap()
            .push((url.to_string(), method_and_body));
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| crate::mcp::Error::transport("no scripted response"))
    }
}

// Taken by value so the many `mcp_json(serde_json::json!({...}))` call sites stay ergonomic.
#[allow(clippy::needless_pass_by_value)]
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
        Self {
            routes: routes
                .into_iter()
                .map(|(u, r)| (u.to_string(), r))
                .collect(),
        }
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
    ) -> crate::mcp::error::Result<crate::mcp::client::HttpResponse> {
        for (pat, resp) in &self.routes {
            if url.contains(pat.as_str()) {
                return Ok(resp.clone());
            }
        }
        Ok(crate::mcp::client::HttpResponse {
            status: 404,
            headers: vec![],
            body: Vec::new(),
        })
    }
}

/// A 200 JSON response (for `FakeGreaseHttp` routes).
// Taken by value so the many `grease_json(serde_json::json!({...}))` call sites stay ergonomic.
#[allow(clippy::needless_pass_by_value)]
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

// `static COUNTER` sits with the per-call sequence number it backs, after the lock acquisition.
#[allow(clippy::items_after_statements)]
fn set_grease_dirs() -> GreaseDirsGuard {
    let lock = crate::grease::config::TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let base = std::env::temp_dir().join(format!("clank_grease_sess_{}_{n}", std::process::id()));
    for sub in [
        "etc",
        "store",
        "bin",
        "script-bin",
        "skills",
        "mnt-mcp",
        "agent-bin",
    ] {
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
// `static COUNTER` sits with the per-call sequence number it backs, after the lock acquisition.
#[allow(clippy::items_after_statements)]
fn set_mcp_dirs() -> McpDirsGuard {
    let lock = crate::mcp::config::TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        Self {
            routes: routes
                .into_iter()
                .map(|(u, r)| (u.to_string(), r))
                .collect(),
        }
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
    ) -> crate::mcp::error::Result<crate::mcp::client::HttpResponse> {
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
            if let Some(uri) = v
                .get("params")
                .and_then(|p| p.get("uri"))
                .and_then(|u| u.as_str())
            {
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
        Ok(mcp_json(
            serde_json::json!({"jsonrpc":"2.0","id":1,"result":{}}),
        ))
    }
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
    ) -> crate::golem::error::Result<String> {
        *self.seen.lock().unwrap() = Some(inv.clone());
        Ok(self.reply.clone())
    }
    async fn invoke_async(
        &self,
        inv: &crate::golem::agent::AgentInvocation,
    ) -> crate::golem::error::Result<crate::golem::agent::InvokeHandle> {
        *self.seen.lock().unwrap() = Some(inv.clone());
        let (token, note) = match &inv.mode {
            crate::golem::agent::InvokeMode::Trigger => (None, "triggered".to_string()),
            crate::golem::agent::InvokeMode::Schedule(w) => {
                (Some("tok".to_string()), format!("scheduled for {w}"))
            }
            crate::golem::agent::InvokeMode::Await => (None, String::new()),
        };
        Ok(crate::golem::agent::InvokeHandle {
            cancel_token: token,
            note,
        })
    }
}

/// A scripted [`crate::golem::cluster::GolemCluster`] recording the calls it saw.
struct FakeGolemCluster;

#[async_trait::async_trait(?Send)]
impl crate::golem::cluster::GolemCluster for FakeGolemCluster {
    async fn agent_list(&self) -> crate::golem::error::Result<String> {
        Ok("agent-1\nagent-2".to_string())
    }
    async fn agent_oplog(
        &self,
        t: &str,
        _c: &[(String, String)],
    ) -> crate::golem::error::Result<String> {
        Ok(format!("oplog for {t}"))
    }
    async fn agent_status(
        &self,
        t: &str,
        _c: &[(String, String)],
    ) -> crate::golem::error::Result<String> {
        Ok(format!("status for {t}"))
    }
    async fn connect(&self, id: &str) -> crate::golem::error::Result<String> {
        Ok(format!("connected to {id}"))
    }
    async fn self_oplog(&self) -> crate::golem::error::Result<String> {
        Ok("self oplog".to_string())
    }
    async fn rollback(&self) -> crate::golem::error::Result<String> {
        Ok("rolled back".to_string())
    }
    async fn fork(&self) -> crate::golem::error::Result<String> {
        Ok("forked".to_string())
    }
}

/// Install a `golem:shopping-cart` agent (helper) — a durable `ShoppingCart` with one method.
async fn install_shopping_cart(session: &mut Session) {
    let pkg = serde_json::json!({
        "kind": "agent", "name": "shopping-cart", "description": "cart",
        "agent-type": "ShoppingCart", "constructor-params": ["userid"],
        "methods": [{"name": "add-item", "params": ["sku"]}], "ephemeral": false
    });
    session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![(
        "/packages/",
        grease_json(pkg),
    )])));
    session
        .run_line("grease registry add https://reg.example")
        .await;
    session.eval_line("sudo grease install shopping-cart").await;
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

/// A `tools/call` response echoing text content.
fn mcp_call_response(text: &str) -> crate::mcp::client::HttpResponse {
    mcp_json(serde_json::json!({"jsonrpc":"2.0","id":9,"result":{
        "content":[{"type":"text","text":text}], "isError":false}}))
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
        std::env::set_var(crate::config::env::LOG_DIR, &dir);
        Self { _lock: lock, dir }
    }
    fn read(&self, file: crate::logging::LogFile) -> String {
        std::fs::read_to_string(self.dir.join(file.filename())).unwrap_or_default()
    }
}

impl Drop for LogCapture {
    fn drop(&mut self) {
        std::env::remove_var(crate::config::env::LOG_DIR);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A seeded temp file for `rm` tests: returns its path. Uses a unique name per test.
fn seed_file(tag: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("clank_authz_{tag}_{}", std::process::id()));
    std::fs::write(&path, b"x").unwrap();
    path
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

/// Serializes the cwd-sensitive `cd` test against the curl-pipeline tests: their grep stages hold
/// process-cwd windows (`ShellCwd`) while a mock server round-trips, and the process cwd is one
/// global across Sessions. Test-parallelism artifact only; production runs one line at a time.
static CWD_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
