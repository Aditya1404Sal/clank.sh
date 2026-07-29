//! MCP: installing a server, the tool/prompt/resource surfaces it registers, session
//! lifecycle, and reconstructing state from the on-disk config cache.
//!
//! Fixtures live in the parent module ([`super`]).

use super::*;

/// The /mnt/mcp virtual-fs: an MCP server with one STATIC resource (readable at install → real
/// file) and one DYNAMIC resource (install read fails → served live). `ls /mnt/mcp/<server>/` lists
/// both; a top-level `cat` of the dynamic resource fetches it live via resources/read; the same cat
/// inside `$()` hits the honest Wall-C stub.
// `init` (the JSON-RPC initialize response) reads close to `inst` (the install result) — clear in context.
#[allow(clippy::similar_names)]
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
        let res_list = mcp_json(
            serde_json::json!({"jsonrpc":"2.0","id":2,"result":{"resources":[
            {"uri":"file:///docs/guide.md","name":"guide"},
            {"uri":"live://metrics/cpu","name":"cpu"}]}}),
        );
        // Static resource: install read succeeds → materialized as a real file.
        let static_read = mcp_json(serde_json::json!({"jsonrpc":"2.0","id":3,"result":{
            "contents":[{"uri":"file:///docs/guide.md","text":"static guide body"}]}}));
        // Dynamic resource: install read FAILS (500) → recorded dynamic; a later live read succeeds.
        let dyn_read_fail = crate::mcp::client::HttpResponse {
            status: 500,
            headers: vec![],
            body: Vec::new(),
        };
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
        session
            .run_line("grease registry add https://reg.example")
            .await;

        let inst = session
            .eval_line("sudo grease install srv --resources")
            .await;
        assert_eq!(
            inst.exit_code,
            0,
            "install stderr: {}",
            String::from_utf8_lossy(&inst.stderr)
        );

        // The static resource is a real file.
        let static_path = crate::grease::config::mcp_mount_dir().join("srv/docs/guide.md");
        assert_eq!(
            std::fs::read_to_string(&static_path).unwrap(),
            "static guide body"
        );

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
        assert_eq!(
            cat.exit_code,
            0,
            "dynamic cat stderr: {}",
            String::from_utf8_lossy(&cat.stderr)
        );
        assert_eq!(String::from_utf8(cat.stdout).unwrap(), "cpu: 42%");

        // The same read inside `$()` does NOT do the live fetch (Wall-C) — Brush's cat finds no
        // real file → nonzero. (We only assert it doesn't crash / doesn't print the live body.)
        let subst = session
            .eval_line("echo $(cat /mnt/mcp/srv/metrics/cpu)")
            .await;
        assert!(
            !String::from_utf8_lossy(&subst.stdout).contains("cpu: 42%"),
            "dynamic read must not run in $()"
        );
    });
}

/// MCP resource templates + `mcp resource info` + `stat`. Install a server with a resource template
/// (`resources/templates/list`) → a `<server>-<name>` executable; running it substitutes the arg
/// into the URI template and reads the constructed resource. `mcp resource info` shows annotations;
/// `ls /mnt/mcp/<server>` lists the template stub.
// `init` (the JSON-RPC initialize response) reads close to `inst` (the install result) — clear in context.
#[allow(clippy::similar_names)]
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
        let res_list = mcp_json(
            serde_json::json!({"jsonrpc":"2.0","id":2,"result":{"resources":[
            {"uri":"file:///repo/README.md","name":"readme","mimeType":"text/markdown",
             "size":42,"annotations":{"lastModified":"2026-01-01T00:00:00Z","priority":0.8,
             "audience":["user","assistant"]}}]}}),
        );
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
        session
            .run_line("grease registry add https://reg.example")
            .await;

        let inst = session
            .eval_line("sudo grease install gh --resources")
            .await;
        assert_eq!(
            inst.exit_code,
            0,
            "install stderr: {}",
            String::from_utf8_lossy(&inst.stderr)
        );
        assert!(
            String::from_utf8(inst.stdout)
                .unwrap()
                .contains("1 templates"),
            "reports templates"
        );

        // The template executable exists (`type`/`ls` see it).
        let ls = String::from_utf8(session.eval_line("ls /mnt/mcp/gh").await.stdout).unwrap();
        assert!(ls.contains("gh-file-lookup"), "template stub listed: {ls}");

        // Running the template with a positional arg substitutes {path} and reads the URI.
        let run = session.eval_line("gh-file-lookup src/main.rs").await;
        assert_eq!(
            run.exit_code,
            0,
            "template run stderr: {}",
            String::from_utf8_lossy(&run.stderr)
        );
        assert_eq!(String::from_utf8(run.stdout).unwrap(), "fn main() {}");

        // `mcp resource info` shows the annotations.
        let info = String::from_utf8(
            session
                .eval_line("mcp resource info /mnt/mcp/gh/repo/README.md")
                .await
                .stdout,
        )
        .unwrap();
        assert!(
            info.contains("2026-01-01"),
            "info shows lastModified: {info}"
        );
        assert!(
            info.contains("priority: 0.8"),
            "info shows priority: {info}"
        );
        assert!(
            info.contains("user,assistant"),
            "info shows audience: {info}"
        );
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
        let res_list = mcp_json(
            serde_json::json!({"jsonrpc":"2.0","id":2,"result":{"resources":[
            {"uri":"metrics://cpu","name":"cpu"}]}}),
        );
        // resources/read for the dynamic uri (install read fails → dynamic; watch reads succeed).
        let read_ok = mcp_json(serde_json::json!({"jsonrpc":"2.0","id":9,"result":{
            "contents":[{"uri":"metrics://cpu","text":"cpu: 10%"}]}}));

        session.set_mcp_http(Box::new(FakeMcpArtifactHttp::new(vec![
            ("/packages/", grease_json(pkg)),
            ("initialize", init),
            ("resources/list", res_list),
            (
                "resources/templates/list",
                mcp_json(
                    serde_json::json!({"jsonrpc":"2.0","id":3,"result":{"resourceTemplates":[]}}),
                ),
            ),
            ("resources/read:metrics://cpu", read_ok),
            (
                "resources/subscribe",
                mcp_json(serde_json::json!({"jsonrpc":"2.0","id":4,"result":{}})),
            ),
        ])));
        session
            .run_line("grease registry add https://reg.example")
            .await;
        session
            .eval_line("sudo grease install metrics --resources")
            .await;

        let watch = session.eval_line("mcp watch metrics://cpu").await;
        assert_eq!(watch.exit_code, 0);
        let out = String::from_utf8(watch.stdout).unwrap();
        assert!(out.contains("bounded poll"), "honest about polling: {out}");
        assert!(
            out.contains("cpu: 10%"),
            "prints the resource content: {out}"
        );
        assert!(out.contains("done"), "terminates: {out}");
    });
}

/// reflect it. Uses a scripted fake transport.
#[test]
fn mcp_add_installs_and_surfaces_the_server() {
    on_rt(async {
        let dirs = set_mcp_dirs();
        let mut session = Session::new().await.unwrap();
        // Put the mcp bin dir on PATH so `which` finds the stub.
        session
            .run_line(&format!("export PATH={}:$PATH", dirs.bin))
            .await;
        session.set_mcp_http(Box::new(FakeMcpHttp::new(mcp_install_script())));

        let add = session
            .eval_line("sudo mcp add demo https://x.example/mcp")
            .await;
        assert_eq!(
            add.exit_code,
            0,
            "add failed: {}",
            String::from_utf8_lossy(&add.stderr)
        );
        assert!(String::from_utf8(add.stdout).unwrap().contains("1 tools"));

        let (list, _) = session.run_line("mcp list").await;
        let list = String::from_utf8(list).unwrap();
        assert!(
            list.contains("demo") && list.contains("1 tools"),
            "got: {list}"
        );

        let (tools, _) = session.run_line("mcp tools demo").await;
        assert!(String::from_utf8(tools).unwrap().contains("echo"));

        // The /usr/lib/mcp/bin stub is a real file, so `which` finds it.
        let (which, _) = session.run_line("which demo").await;
        assert!(
            String::from_utf8(which).unwrap().contains("demo"),
            "which should find the stub"
        );
    });
}

/// Native cross-restart MCP DISPATCH: after a restart (a fresh Session reconstructs the server from
/// the config cache), a live tool call works with the transport re-injected — WITHOUT `mcp reload`.
/// Dispatch is a stateless `tools/call` from the reconstructed config URL + `mcp_http`; it needs no
/// open session (`session_id` falls back to None). This pins that the pre-cache "needs reload" caveat
/// no longer applies.
#[test]
fn mcp_dispatch_survives_restart_without_reload() {
    on_rt(async {
        let _dirs = set_mcp_dirs();
        {
            // Session 1: install `demo`, caching its tools in the on-disk config.
            let mut session = Session::new().await.unwrap();
            session.set_mcp_http(Box::new(FakeMcpHttp::new(mcp_install_script())));
            let add = session.eval_line("sudo mcp add demo https://x/mcp").await;
            assert_eq!(
                add.exit_code,
                0,
                "add: {}",
                String::from_utf8_lossy(&add.stderr)
            );
        }
        // Restart: a brand-new Session (reconstruction runs), transport re-injected as native::run
        // does AFTER Session::new. Serve ONLY the tools/call response — no re-initialize.
        let mut fresh = Session::new().await.unwrap();
        fresh.set_mcp_http(Box::new(FakeMcpHttp::new(vec![mcp_call_response(
            "echoed: hi",
        )])));

        // Dispatch WITHOUT `mcp reload`.
        let out = fresh.eval_line("sudo demo echo --text hi").await;
        assert_eq!(
            out.exit_code,
            0,
            "dispatch: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(String::from_utf8(out.stdout)
            .unwrap()
            .contains("echoed: hi"));
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
            let add = session
                .eval_line("sudo mcp add demo https://x.example/mcp")
                .await;
            assert_eq!(
                add.exit_code,
                0,
                "add failed: {}",
                String::from_utf8_lossy(&add.stderr)
            );
        }
        // A brand-new Session, same CLANK_MCP_ETC, no HTTP transport at all.
        let mut fresh = Session::new().await.unwrap();
        let (list, _) = fresh.run_line("mcp list").await;
        let list = String::from_utf8(list).unwrap();
        assert!(
            list.contains("demo"),
            "reconstructed server missing: {list}"
        );
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
        let bad = crate::mcp::client::HttpResponse {
            status: 500,
            headers: vec![],
            body: vec![],
        };
        session.set_mcp_http(Box::new(FakeMcpHttp::new(vec![bad])));

        let add = session
            .eval_line("sudo mcp add demo https://x.example/mcp")
            .await;
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
        assert_eq!(
            out.exit_code,
            0,
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(String::from_utf8(out.stdout)
            .unwrap()
            .contains("echoed: hello"));
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
        assert!(
            first.pending_prompt.is_some(),
            "MCP tool call should confirm"
        );
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
        assert!(
            String::from_utf8(man).unwrap().contains("demo"),
            "man should resolve the server"
        );
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

        let out = session
            .eval_line(r#"sudo demo echo --args '{"text":"direct"}'"#)
            .await;
        assert_eq!(
            out.exit_code,
            0,
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        // The tools/call body carried the raw args verbatim.
        let calls = seen.lock().unwrap();
        let last = calls.last().unwrap();
        assert!(
            last.1.contains("\"text\":\"direct\""),
            "tools/call body: {}",
            last.1
        );
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
        open_init
            .headers
            .push(("mcp-session-id".into(), "srv-open".into()));
        script.push(open_init);
        script.push(mcp_json(serde_json::json!({}))); // initialized
        script.push(mcp_json(serde_json::json!({}))); // DELETE close (200)
        session.set_mcp_http(Box::new(FakeMcpHttp::new(script)));
        session.eval_line("sudo mcp add demo https://x/mcp").await;

        // No sessions yet.
        let (list0, _) = session.run_line("mcp session list").await;
        assert!(String::from_utf8(list0)
            .unwrap()
            .contains("no open MCP sessions"));

        // Open one.
        let open = session.eval_line("sudo mcp session open demo").await;
        assert_eq!(
            open.exit_code,
            0,
            "stderr: {}",
            String::from_utf8_lossy(&open.stderr)
        );
        assert!(String::from_utf8(open.stdout).unwrap().contains("s1"));

        let (list1, _) = session.run_line("mcp session list").await;
        let list1 = String::from_utf8(list1).unwrap();
        assert!(
            list1.contains("s1") && list1.contains("srv-open"),
            "got: {list1}"
        );

        let (info, _) = session.run_line("mcp session info s1").await;
        assert!(String::from_utf8(info).unwrap().contains("demo"));

        // Close it.
        let close = session.eval_line("sudo mcp session close s1").await;
        assert_eq!(close.exit_code, 0);
        let (list2, _) = session.run_line("mcp session list").await;
        assert!(String::from_utf8(list2)
            .unwrap()
            .contains("no open MCP sessions"));
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
        open_init
            .headers
            .push(("mcp-session-id".into(), "srv-open".into()));
        script.push(open_init);
        script.push(mcp_json(serde_json::json!({}))); // initialized
        script.push(crate::mcp::client::HttpResponse {
            status: 405,
            headers: vec![],
            body: vec![],
        });
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
        assert!(
            combined.contains("405") || combined.contains("locally"),
            "got: {combined}"
        );
        let (list, _) = session.run_line("mcp session list").await;
        assert!(String::from_utf8(list)
            .unwrap()
            .contains("no open MCP sessions"));
    });
}

/// C4: an installed MCP tool becomes an ask `ToolDefinition`. Under `sudo ask`, the model calling
/// `mcp__demo__echo` runs the tool (blanket confirm-tier) and the `FakeMcpHttp` sees the tools/call.
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

        let result = session
            .eval_line(r#"sudo ask "use the demo echo tool""#)
            .await;
        assert_eq!(
            result.exit_code,
            0,
            "stderr: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(
            String::from_utf8(result.stdout).unwrap(),
            "done, the tool ran"
        );
        // The MCP server saw a tools/call carrying the tool's arguments.
        let calls = seen.lock().unwrap();
        let tool_call = calls
            .iter()
            .find(|(_url, body)| body.contains("tools/call"));
        assert!(tool_call.is_some(), "expected a tools/call, saw: {calls:?}");
        assert!(
            tool_call.unwrap().1.contains("hi mcp"),
            "args should reach the server"
        );
        // The tool surface the model saw included the MCP tool definition.
        let ask_turns = ask_seen.lock().unwrap();
        assert!(
            ask_turns[0]
                .tools
                .iter()
                .any(|t| t.name == "mcp__demo__echo"),
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
        assert!(
            after_ask.pending_prompt.is_some(),
            "MCP tool call should pause under plain ask"
        );
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
        assert!(String::from_utf8(result.stderr)
            .unwrap()
            .contains("no installed server owns"));
    });
}
