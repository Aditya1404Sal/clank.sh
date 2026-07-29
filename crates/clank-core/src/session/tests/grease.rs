//! `grease`: the five package kinds, and the integrity chain (sha256 → signature →
//! inclusion proof) that decides whether a package is allowed to install at all.
//!
//! Fixtures live in the parent module ([`super`]).

use super::*;

/// End-to-end: `grease install <server>` for a `kind:mcp` package fetches the server's live surface
/// (initialize + tools/list + prompts/list + prompts/get + resources/list/read), registers the
/// tools into `McpState` (so `<server> <tool>` works), materializes prompts as $PATH commands and
/// static resources under /mnt/mcp, and caches the surface so a fresh Session rebuilds it offline.
// `init` (the JSON-RPC initialize response) reads close to `inst` (the install result) — clear in context.
#[allow(clippy::similar_names)]
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
        session
            .run_line("grease registry add https://reg.example")
            .await;

        let inst = session.eval_line("sudo grease install demo").await;
        assert_eq!(
            inst.exit_code,
            0,
            "install stderr: {}",
            String::from_utf8_lossy(&inst.stderr)
        );
        let out = String::from_utf8(inst.stdout).unwrap();
        assert!(
            out.contains("installed demo [mcp]"),
            "install output: {out}"
        );
        assert!(
            out.contains("1 tools") && out.contains("1 prompts"),
            "counts: {out}"
        );

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
        assert!(
            info.contains("[mcp]") && info.contains("https://mcp.demo/x"),
            "info: {info}"
        );
        assert!(
            info.contains("echo") && info.contains("summarize-diff"),
            "info lists artifacts: {info}"
        );

        // A FRESH Session rebuilds the tool surface from the cached payload (no live fetch).
        let session2 = Session::new().await.unwrap();
        assert!(
            session2.is_mcp_tool_line("demo echo --text hi"),
            "boot reconstruction failed"
        );

        // Remove deregisters from McpState + deletes the resource mount.
        let rm = session.eval_line("sudo grease remove demo").await;
        assert_eq!(rm.exit_code, 0);
        assert!(!session.grease.is_mcp("demo"));
        assert!(!session.is_mcp_tool_line("demo echo --text hi"));
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
        assert!(String::from_utf8(list0.stdout)
            .unwrap()
            .contains("no registries configured"));

        let add = session
            .eval_line("grease registry add https://reg.example")
            .await;
        assert_eq!(add.exit_code, 0);
        assert!(
            add.pending_prompt.is_none(),
            "registry add is Allow — no pause"
        );
        assert!(String::from_utf8(add.stdout)
            .unwrap()
            .contains("added registry"));

        let list1 = session.eval_line("grease registry list").await;
        assert!(String::from_utf8(list1.stdout)
            .unwrap()
            .contains("https://reg.example"));

        let rm = session
            .eval_line("grease registry remove https://reg.example")
            .await;
        assert_eq!(rm.exit_code, 0);
        let list2 = session.eval_line("grease registry list").await;
        assert!(String::from_utf8(list2.stdout)
            .unwrap()
            .contains("no registries configured"));
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
        session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![(
            "/packages/",
            grease_json(pkg),
        )])));
        session
            .run_line("grease registry add https://reg.example")
            .await;

        // Install (sudo pre-authorizes the Confirm).
        let inst = session.eval_line("sudo grease install tldr").await;
        assert_eq!(
            inst.exit_code,
            0,
            "install stderr: {}",
            String::from_utf8_lossy(&inst.stderr)
        );
        assert!(String::from_utf8(inst.stdout)
            .unwrap()
            .contains("installed tldr"));

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
        assert!(String::from_utf8(miss.stderr)
            .unwrap()
            .contains("missing required argument --file"));

        // Run it with the arg (sudo pre-authorizes the prompt's Confirm) → the model sees the
        // FILLED body.
        let run = session.eval_line("sudo tldr --file report.md").await;
        assert_eq!(run.exit_code, 0);
        assert_eq!(String::from_utf8(run.stdout).unwrap(), "the summary");
        let content = seen.lock().unwrap()[0].user_content();
        assert!(
            content.contains("Summarize the file report.md concisely."),
            "got: {content}"
        );

        // A bare (non-sudo) prompt run confirms (outbound LLM).
        let confirm = session.eval_line("tldr --file x.md").await;
        assert!(
            confirm.pending_prompt.is_some(),
            "prompt run should confirm without sudo"
        );
        session.answer_prompt(Some("no".into())).await;

        // Remove deregisters: the name is no longer an installed prompt.
        let rm = session.eval_line("sudo grease remove tldr").await;
        assert_eq!(rm.exit_code, 0);
        assert!(!session.grease.is_prompt("tldr"));
    });
}

/// A prompt authored as a `.md` file with YAML frontmatter installs identically to a JSON prompt:
/// grease fetches `/packages/<name>.md`, converts the frontmatter → the canonical `PromptPackage`,
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
        session
            .run_line("grease registry add https://reg.example")
            .await;

        let inst = session.eval_line("sudo grease install tldr").await;
        assert_eq!(
            inst.exit_code,
            0,
            "install stderr: {}",
            String::from_utf8_lossy(&inst.stderr)
        );
        assert!(String::from_utf8(inst.stdout)
            .unwrap()
            .contains("installed tldr"));

        // Installed as a prompt with the declared arg (from the frontmatter).
        let help = session.eval_line("tldr --help").await;
        assert_eq!(help.exit_code, 0);
        assert!(String::from_utf8(help.stdout).unwrap().contains("--file"));

        // Running fills the body (converted from the `.md` body) and dispatches to the model.
        let run = session.eval_line("sudo tldr --file report.md").await;
        assert_eq!(run.exit_code, 0);
        assert_eq!(String::from_utf8(run.stdout).unwrap(), "the summary");
        let content = seen.lock().unwrap()[0].user_content();
        assert!(
            content.contains("Summarize the file report.md concisely."),
            "got: {content}"
        );
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
            session
                .eval_line("cat /proc/clank/system-prompt")
                .await
                .stdout,
        )
        .unwrap();
        assert!(!before.contains("prompt__hello"), "not installed yet");

        let pkg = serde_json::json!({
            "kind": "prompt", "name": "hello", "description": "say hi", "body": "Say hi."
        });
        session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![(
            "/packages/",
            grease_json(pkg),
        )])));
        session
            .run_line("grease registry add https://reg.example")
            .await;
        session.eval_line("sudo grease install hello").await;

        // After install: the proc file lists the installed prompt tool + the "Installed prompt
        // tools" heading from build_system_prompt_with_capabilities.
        let after = String::from_utf8(
            session
                .eval_line("cat /proc/clank/system-prompt")
                .await
                .stdout,
        )
        .unwrap();
        assert!(
            after.contains("prompt__hello"),
            "system prompt lists the installed prompt: {after}"
        );
        assert!(
            after.contains("Installed prompt tools"),
            "and its heading: {after}"
        );
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
        session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![(
            "/packages/",
            grease_json(pkg),
        )])));
        session
            .run_line("grease registry add https://reg.example")
            .await;

        let inst = session.eval_line("sudo grease install greet").await;
        assert_eq!(
            inst.exit_code,
            0,
            "install stderr: {}",
            String::from_utf8_lossy(&inst.stderr)
        );
        let out = String::from_utf8(inst.stdout).unwrap();
        assert!(out.contains("installed greet"), "install output: {out}");
        assert!(
            out.contains("[script]"),
            "install output names the kind: {out}"
        );

        // It's an installed SCRIPT (not a prompt), and its stub is in the script bin dir.
        assert!(session.grease.is_script("greet"));
        assert!(!session.grease.is_prompt("greet"));
        assert!(crate::grease::config::script_bin_dir()
            .join("greet")
            .exists());
        assert!(!crate::grease::config::bin_dir().join("greet").exists());

        // `grease list` shows it tagged as a script.
        let list = String::from_utf8(session.eval_line("grease list").await.stdout).unwrap();
        assert!(
            list.contains("greet") && list.contains("[script]"),
            "list: {list}"
        );

        // `greet --help` shows generated help disclosing the local-shell capability, no confirm.
        let help = session.eval_line("greet --help").await;
        assert_eq!(help.exit_code, 0);
        let help_s = String::from_utf8(help.stdout).unwrap();
        assert!(help_s.contains("--who"), "help: {help_s}");
        assert!(
            help_s.contains("local shell"),
            "help discloses shell capability: {help_s}"
        );

        // Missing required arg → exit 2, no shell run.
        let miss = session.eval_line("sudo greet").await;
        assert_eq!(miss.exit_code, 2);
        assert!(String::from_utf8(miss.stderr)
            .unwrap()
            .contains("missing required argument --who"));

        // Run it (sudo pre-authorizes the Confirm) → the FILLED shell body runs locally.
        let run = session.eval_line("sudo greet --who world").await;
        assert_eq!(
            run.exit_code,
            0,
            "run stderr: {}",
            String::from_utf8_lossy(&run.stderr)
        );
        assert_eq!(
            String::from_utf8(run.stdout).unwrap().trim_end(),
            "hello world"
        );

        // A bare (non-sudo) script run confirms (running local shell is a Confirm capability).
        let confirm = session.eval_line("greet --who x").await;
        assert!(
            confirm.pending_prompt.is_some(),
            "script run should confirm without sudo"
        );
        session.answer_prompt(Some("no".into())).await;

        // Remove deregisters and deletes the script stub.
        let rm = session.eval_line("sudo grease remove greet").await;
        assert_eq!(rm.exit_code, 0);
        assert!(!session.grease.is_script("greet"));
        assert!(!crate::grease::config::script_bin_dir()
            .join("greet")
            .exists());
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
        session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![(
            "/packages/",
            grease_json(pkg),
        )])));
        session
            .run_line("grease registry add https://reg.example")
            .await;

        let inst = session.eval_line("sudo grease install code-review").await;
        assert_eq!(
            inst.exit_code,
            0,
            "install stderr: {}",
            String::from_utf8_lossy(&inst.stderr)
        );
        let out = String::from_utf8(inst.stdout).unwrap();
        assert!(
            out.contains("installed code-review") && out.contains("[skill]"),
            "out: {out}"
        );

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
        let info =
            String::from_utf8(session.eval_line("grease info code-review").await.stdout).unwrap();
        assert!(
            info.contains("[skill]") && info.contains("SKILL.md") && info.contains("lint-all"),
            "info: {info}"
        );

        // The skill is surfaced in the agentic system prompt (context, not a callable tool).
        let sys = crate::ai::ask::build_system_prompt_with_capabilities(
            &session.registry,
            &session.mcp,
            &session.grease,
        );
        assert!(
            sys.contains("Installed skills"),
            "system prompt lists skills: …"
        );
        assert!(
            sys.contains("code-review") && sys.contains("when the user asks for a code review")
        );

        // Remove deletes the dir tree and deregisters.
        let rm = session.eval_line("sudo grease remove code-review").await;
        assert_eq!(rm.exit_code, 0);
        assert!(!session.grease.is_skill("code-review"));
        assert!(!skill_root.exists());
    });
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
            (
                "/packages/",
                crate::mcp::client::HttpResponse {
                    status: 200,
                    headers: vec![("content-type".into(), "application/json".into())],
                    body,
                },
            ),
        ])));
        session
            .run_line(&format!(
                "grease registry add https://reg.example --key {pubkey}"
            ))
            .await;

        let inst = session.eval_line("sudo grease install signed-pkg").await;
        assert_eq!(
            inst.exit_code,
            0,
            "install stderr: {}",
            String::from_utf8_lossy(&inst.stderr)
        );
        let out = String::from_utf8(inst.stdout).unwrap();
        assert!(out.contains("signed"), "install reports signed: {out}");
        assert!(session.grease.is_prompt("signed-pkg"));
        // `grease info` shows the signer.
        let info =
            String::from_utf8(session.eval_line("grease info signed-pkg").await.stdout).unwrap();
        assert!(
            info.contains("signed by alice"),
            "info shows signer: {info}"
        );
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
            (
                "/packages/",
                crate::mcp::client::HttpResponse {
                    status: 200,
                    headers: vec![("content-type".into(), "application/json".into())],
                    body,
                },
            ),
        ])));
        session
            .run_line(&format!(
                "grease registry add https://reg.example --key {pubkey}"
            ))
            .await;

        let inst = session.eval_line("sudo grease install bad-sig").await;
        assert_eq!(inst.exit_code, 4, "a bad signature must reject");
        assert!(String::from_utf8(inst.stderr)
            .unwrap()
            .contains("signature verification failed"));
        assert!(
            !session.grease.is_prompt("bad-sig"),
            "nothing installed on sig failure"
        );
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
            (
                "/packages/",
                crate::mcp::client::HttpResponse {
                    status: 200,
                    headers: vec![("content-type".into(), "application/json".into())],
                    body,
                },
            ),
        ])));
        session
            .run_line(&format!(
                "grease registry add https://reg.example --key {pubkey}"
            ))
            .await;

        let inst = session.eval_line("sudo grease install nosig").await;
        assert_eq!(inst.exit_code, 4);
        assert!(String::from_utf8(inst.stderr)
            .unwrap()
            .contains("no signature"));
        assert!(!session.grease.is_prompt("nosig"));
    });
}

/// An `UNsigned` registry (no `--key`) still installs, marked unsigned (record-only signing).
#[test]
fn grease_install_from_unsigned_registry_is_record_only() {
    on_rt(async {
        let _dirs = set_grease_dirs();
        let mut session = Session::new().await.unwrap();
        let pkg = serde_json::json!({
            "kind": "prompt", "name": "plain", "description": "d", "body": "hi"
        });
        session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![(
            "/packages/",
            grease_json(pkg),
        )])));
        session
            .run_line("grease registry add https://reg.example")
            .await; // no --key

        let inst = session.eval_line("sudo grease install plain").await;
        assert_eq!(
            inst.exit_code,
            0,
            "install stderr: {}",
            String::from_utf8_lossy(&inst.stderr)
        );
        assert!(session.grease.is_prompt("plain"));
        let info = String::from_utf8(session.eval_line("grease info plain").await.stdout).unwrap();
        assert!(info.contains("unsigned"), "info shows unsigned: {info}");
    });
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
            (
                "/packages/",
                crate::mcp::client::HttpResponse {
                    status: 200,
                    headers: vec![("content-type".into(), "application/json".into())],
                    body,
                },
            ),
        ])));
        session
            .run_line(&format!(
                "grease registry add https://reg.example --key {pubkey}"
            ))
            .await;

        let inst = session.eval_line("sudo grease install logged").await;
        assert_eq!(
            inst.exit_code,
            0,
            "install stderr: {}",
            String::from_utf8_lossy(&inst.stderr)
        );
        // The proof was checked, and that is ALL the wording claims. It deliberately no longer says
        // "in log": the signature covers only the payload body, so the index entry carrying the log
        // root is unauthenticated and the tree is single-leaf — the proof reduces to a hash of the
        // payload against a root the same party supplied. Calling that "in transparency log"
        // overstated it to the two readers least able to check: the user skimming `grease info` and
        // the model reading it as context.
        let stdout = String::from_utf8(inst.stdout).unwrap();
        assert!(
            stdout.contains("log proof (registry-asserted root)"),
            "reports the proof without overclaiming: {stdout}"
        );
        let info = String::from_utf8(session.eval_line("grease info logged").await.stdout).unwrap();
        assert!(
            info.contains("log proof @0 (registry-asserted root)"),
            "info shows the log index, qualified: {info}"
        );
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
            (
                "/packages/",
                crate::mcp::client::HttpResponse {
                    status: 200,
                    headers: vec![("content-type".into(), "application/json".into())],
                    body,
                },
            ),
        ])));
        session
            .run_line(&format!(
                "grease registry add https://reg.example --key {pubkey}"
            ))
            .await;

        let inst = session.eval_line("sudo grease install badlog").await;
        assert_eq!(inst.exit_code, 4, "a bad log proof must reject");
        assert!(String::from_utf8(inst.stderr)
            .unwrap()
            .contains("transparency-log check failed"));
        assert!(
            !session.grease.is_prompt("badlog"),
            "nothing installed on log failure"
        );
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
        session
            .run_line("grease registry add https://reg.example/pkgs")
            .await;

        let surface = session.eval_line("grease install tldr").await;
        let q = surface
            .pending_prompt
            .expect("install should confirm")
            .question;
        assert!(q.contains("\"tldr\""), "discloses the package name: {q}");
        assert!(
            q.contains("https://reg.example/pkgs"),
            "discloses the source registry: {q}"
        );
        assert!(
            q.contains("run via ask"),
            "discloses the ask capability: {q}"
        );
        assert!(
            q.contains("local shell"),
            "discloses the local-shell capability: {q}"
        );
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
        session
            .run_line("grease registry add https://reg.example")
            .await;
        let inst = session.eval_line("sudo grease install vpkg").await;
        assert_eq!(
            inst.exit_code,
            0,
            "stderr: {}",
            String::from_utf8_lossy(&inst.stderr)
        );
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
        session
            .run_line("grease registry add https://reg.example")
            .await;
        let inst = session.eval_line("sudo grease install vpkg").await;
        assert_eq!(inst.exit_code, 4);
        assert!(String::from_utf8(inst.stderr)
            .unwrap()
            .contains("integrity check failed"));
        assert!(
            !session.grease.is_prompt("vpkg"),
            "a mismatched package must not install"
        );
        assert!(!crate::grease::config::store_dir().join("vpkg").exists());
    });
}

/// A registry index that LISTS a package but omits its sha256 is REFUSED (the tamper vector: a
/// stripped hash must not silently bypass content-addressing). Every indexed package must be hashed.
#[test]
fn grease_install_rejects_indexed_package_without_hash() {
    on_rt(async {
        let _dirs = set_grease_dirs();
        let mut session = Session::new().await.unwrap();
        let pkg = serde_json::json!({"name":"loose","description":"d","body":"hi."});
        // Index present but with no sha256 field for the package → refuse.
        let index = serde_json::json!({"packages":[{"name":"loose","description":"d"}]});
        session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![
            ("/index.json", grease_json(index)),
            ("/packages/", grease_json(pkg)),
        ])));
        session
            .run_line("grease registry add https://reg.example")
            .await;
        let inst = session.eval_line("sudo grease install loose").await;
        assert_eq!(inst.exit_code, 4, "indexed-but-unhashed must be refused");
        let err = String::from_utf8(inst.stderr).unwrap();
        assert!(err.contains("without a sha256"), "got: {err}");
        assert!(!session.grease.is_prompt("loose"), "must not install");
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
        session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![(
            "/packages/",
            grease_json(pkg),
        )])));
        session
            .run_line("grease registry add https://reg.example")
            .await;
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

        let result = session
            .eval_line(r#"sudo ask "summarize report.md with the tldr prompt""#)
            .await;
        assert_eq!(
            result.exit_code,
            0,
            "stderr: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(String::from_utf8(result.stdout).unwrap(), "summarized it");

        let turns = ask_seen.lock().unwrap();
        // The prompt tool was in the outer ask's tool surface AND listed in its system prompt.
        assert!(
            turns[0].tools.iter().any(|t| t.name == "prompt__tldr"),
            "the prompt should be an ask tool"
        );
        let system = turns[0].system.clone().unwrap_or_default();
        assert!(
            system.contains("prompt__tldr"),
            "system prompt should list the prompt tool"
        );
        // The nested prompt run saw the FILLED body (turn 2 — the {{file}} was substituted).
        let saw_filled = turns
            .iter()
            .any(|t| t.user_content().contains("TL;DR of report.md please."));
        assert!(
            saw_filled,
            "the model should have seen the filled prompt body"
        );
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
        session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![(
            "/packages/",
            grease_json(pkg),
        )])));
        session
            .run_line("grease registry add https://reg.example")
            .await;
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
        assert!(
            r.pending_prompt.is_some(),
            "a plain ask + prompt tool call should pause"
        );
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
        session
            .run_line("grease registry add https://reg.example")
            .await;
        // `ask` is a builtin — installing a package named `ask` must fail before any fetch.
        let r = session.eval_line("sudo grease install ask").await;
        assert_eq!(r.exit_code, 2);
        assert!(String::from_utf8(r.stderr)
            .unwrap()
            .contains("collides with a built-in"));
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
        assert!(String::from_utf8(bad.stderr)
            .unwrap()
            .contains("not an absolute http(s) URL"));

        // Plain http to a remote host is refused: the transport offers no integrity underneath the
        // signature checks, and one confirmed install from it lands a script on $PATH.
        let cleartext = session
            .eval_line("grease registry add http://attacker.example")
            .await;
        assert_eq!(cleartext.exit_code, 2);
        assert!(String::from_utf8(cleartext.stderr)
            .unwrap()
            .contains("plain http"));

        // Loopback is exempt — it is the documented `grease-populate` dev workflow.
        let local = session
            .eval_line("grease registry add http://localhost:8823")
            .await;
        assert_eq!(
            local.exit_code,
            0,
            "stderr: {}",
            String::from_utf8_lossy(&local.stderr)
        );
        session
            .eval_line("grease registry remove http://localhost:8823")
            .await;

        // `install` is Confirm — a bare invocation pauses.
        let confirm = session.eval_line("grease install summarize").await;
        assert!(
            confirm.pending_prompt.is_some(),
            "install should confirm without sudo"
        );
        session.answer_prompt(Some("no".into())).await;

        // Under sudo it runs; with no registry configured it errors honestly (no panic).
        let inst = session.eval_line("sudo grease install summarize").await;
        assert_eq!(inst.exit_code, 1);
        assert!(String::from_utf8(inst.stderr)
            .unwrap()
            .contains("no registries configured"));
    });
}

/// A rejected install leaves evidence in ops.log.
///
/// Regression: nothing in the install pipeline reached any log. A sha256 mismatch, a bad signature
/// or a failed inclusion proof wrote to the terminal and vanished — and on the agent the terminal
/// output is gone the moment the invocation returns, so a supply-chain rejection left no trace
/// anyone could find afterwards. Only the registry's HTTP fetches showed up, in http.log.
#[test]
fn a_failed_grease_install_is_audited_to_ops_log() {
    on_rt(async {
        let _dirs = set_grease_dirs();
        let cap = LogCapture::new("grease-audit");
        let mut session = Session::new().await.unwrap();
        // No registries configured → the install fails before any fetch.
        let r = session.eval_line("sudo grease install nope").await;
        assert_ne!(r.exit_code, 0);

        let log = cap.read(crate::logging::LogFile::Ops);
        assert!(log.contains("grease"), "got:\n{log}");
        assert!(log.contains("op=install"), "got:\n{log}");
        assert!(log.contains("package=nope"), "got:\n{log}");
        assert!(log.contains("outcome=failed"), "got:\n{log}");
    });
}

/// A half-installed package is reported and recoverable, not silently invisible.
///
/// Regression: `load_one` was six `.ok()?`s, so a marker whose payload was missing or corrupt
/// vanished from `grease list` while the marker file survived — and `grease remove` then said "is
/// not installed", leaving no way to clean it up. The digest recorded in the marker was also never
/// re-checked after install, so `grease info` kept printing "verified" for a tampered payload.
#[test]
fn a_half_installed_package_is_reported_and_can_be_removed() {
    on_rt(async {
        let _dirs = set_grease_dirs();
        // A marker with no payload at all — the state a crash between the two writes leaves.
        let etc = crate::grease::config::etc_dir();
        std::fs::write(
            etc.join("ghost.toml"),
            "kind = \"prompt\"\nregistry = \"https://reg.example\"\nsha256 = \"\"\n\
             verified = false\nsignature_verified = false\nlog_verified = false\n",
        )
        .unwrap();

        let mut session = Session::new().await.unwrap();
        assert_eq!(
            session.grease.broken().len(),
            1,
            "the orphan marker must be detected, not skipped"
        );

        // `grease list` names it and reports failure so a driver notices.
        let listed = session.eval_line("grease list").await;
        assert_ne!(listed.exit_code, 0, "a broken install is not a clean list");
        let stdout = String::from_utf8(listed.stdout).unwrap();
        assert!(stdout.contains("ghost"), "got {stdout:?}");
        assert!(stdout.contains("[broken]"), "got {stdout:?}");

        // And the obvious recovery works instead of denying the package exists.
        // `sudo` because grease remove is Confirm-tier; without it the line pauses for authorization.
        let removed = session.eval_line("sudo grease remove ghost").await;
        assert_eq!(
            removed.exit_code,
            0,
            "stdout: {} stderr: {}",
            String::from_utf8_lossy(&removed.stdout),
            String::from_utf8_lossy(&removed.stderr)
        );
        assert!(!etc.join("ghost.toml").exists(), "marker must be gone");
        assert!(session.grease.broken().is_empty());
    });
}

/// A corrupt payload is reported as broken rather than dropped.
#[test]
fn a_corrupt_payload_is_reported_not_silently_skipped() {
    on_rt(async {
        let _dirs = set_grease_dirs();
        let etc = crate::grease::config::etc_dir();
        let store = crate::grease::config::store_dir().join("corrupt");
        std::fs::create_dir_all(&store).unwrap();
        // Truncated mid-write — what a crash during persist_package leaves behind.
        std::fs::write(store.join("prompt.json"), b"{\"name\": \"corr").unwrap();
        std::fs::write(
            etc.join("corrupt.toml"),
            "kind = \"prompt\"\nregistry = \"https://reg.example\"\nsha256 = \"\"\n\
             verified = false\nsignature_verified = false\nlog_verified = false\n",
        )
        .unwrap();

        let session = Session::new().await.unwrap();
        let broken = session.grease.broken();
        assert_eq!(broken.len(), 1, "a corrupt payload must be reported");
        assert!(
            broken[0].1.contains("not a valid prompt package"),
            "got {:?}",
            broken[0].1
        );
    });
}

/// `grease search` must not answer "no packages match" when it could not read a single registry.
///
/// Regression: the fetch, the status check and the JSON parse were all `if let Ok(..)` with no else,
/// so a dead registry produced the same "no packages match '<query>'" at exit 0 as a genuine empty
/// result. A model searching an unreachable registry concluded the package did not exist and gave
/// up. "I found nothing" and "I could not look" are different answers.
#[test]
fn grease_search_separates_an_unreadable_registry_from_a_real_no_match() {
    on_rt(async {
        let _dirs = set_grease_dirs();
        let mut session = Session::new().await.unwrap();
        // No routes → every index.json fetch 404s.
        session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![])));
        session
            .run_line("grease registry add https://reg.example")
            .await;

        let r = session.eval_line("grease search anything").await;
        assert_eq!(
            r.exit_code, 4,
            "an unreadable registry must not look like a clean no-match"
        );
        let err = String::from_utf8(r.stderr).unwrap();
        assert!(err.contains("could not read registry"), "got {err:?}");
        assert!(
            !String::from_utf8(r.stdout).unwrap().contains("no packages"),
            "must not claim the package is absent"
        );
    });
}

/// The counterpart: a registry that ANSWERS but holds no match is a real empty result — exit 0.
#[test]
fn grease_search_reports_a_genuine_no_match_as_success() {
    on_rt(async {
        let _dirs = set_grease_dirs();
        let mut session = Session::new().await.unwrap();
        session.set_mcp_http(Box::new(FakeGreaseHttp::new(vec![(
            "/index.json",
            grease_json(serde_json::json!({"packages": []})),
        )])));
        session
            .run_line("grease registry add https://reg.example")
            .await;

        let r = session.eval_line("grease search anything").await;
        assert_eq!(r.exit_code, 0, "an answered-but-empty search is success");
        assert!(String::from_utf8(r.stdout)
            .unwrap()
            .contains("no packages match"));
    });
}
