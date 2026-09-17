use super::*;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
struct FakeInvoker(Arc<AtomicUsize>);
#[async_trait::async_trait(?Send)]
impl ToolInvoker for FakeInvoker {
    async fn invoke(&self, request: ToolRequest) -> Result<ToolOutput, ToolFailure> {
        self.invoke_blocking(request)
    }
    fn invoke_blocking(&self, _: ToolRequest) -> Result<ToolOutput, ToolFailure> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput {
            stdout: b"invoked\n".to_vec(),
            ..Default::default()
        })
    }
}
fn definition(name: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        version: "1".into(),
        nodes: vec![
            ToolNode {
                name: name.into(),
                aliases: vec![],
                doc: "fixture".into(),
                children: vec![1, 2],
                globals: vec![],
                command: None,
            },
            ToolNode {
                name: "read".into(),
                aliases: vec!["r".into()],
                doc: "read".into(),
                children: vec![],
                globals: vec![],
                command: Some(command(true, "read")),
            },
            ToolNode {
                name: "destroy".into(),
                aliases: vec![],
                doc: "destroy".into(),
                children: vec![],
                globals: vec![],
                command: Some(command(false, "destroy")),
            },
        ],
    }
}
fn command(read_only: bool, path: &str) -> ToolCommand {
    ToolCommand {
        path: vec![path.into()],
        fields: vec![],
        schema: json!({}),
        constraints: vec![],
        read_only,
        stdin: false,
        stdout: false,
        errors: BTreeMap::new(),
        metadata: json!({}),
    }
}
#[test]
fn dispatch_enforces_policy_without_inheriting_hidden_confirmation() {
    crate::test_support::on_rt(async {
        let count = Arc::new(AtomicUsize::new(0));
        let mut session = crate::session::Session::new().await.unwrap();
        session.set_tools(
            ToolRuntime::new(
                vec![definition("remote"), definition("echo")],
                Arc::new(FakeInvoker(count.clone())),
            )
            .unwrap(),
        );
        assert!(session.registry().contains("remote"));
        assert_eq!(session.eval_line("remote r").await.stdout, b"invoked\n");
        assert!(session
            .eval_line("remote destroy")
            .await
            .pending_prompt
            .is_some());
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert_eq!(session.answer_prompt(Some("yes".into())).await.exit_code, 0);
        assert_eq!(count.load(Ordering::SeqCst), 2);
        assert_eq!(
            session
                .eval_line("f() { remote destroy; }; f")
                .await
                .exit_code,
            3
        );
        assert_eq!(
            session.eval_line("eval 'remote destroy'").await.exit_code,
            3
        );
        assert_eq!(count.load(Ordering::SeqCst), 2);
        assert_eq!(
            session.eval_line("echo compiled-wins").await.stdout,
            b"compiled-wins\n"
        );
        assert_eq!(count.load(Ordering::SeqCst), 2);
        assert!(
            String::from_utf8(session.eval_line("type echo").await.stdout)
                .unwrap()
                .contains("shadowed")
        );
    });
}

#[test]
fn invalid_command_graphs_are_rejected_before_help_or_dispatch() {
    let count = Arc::new(AtomicUsize::new(0));
    for bad_child in [0, 99] {
        let mut tool = definition("remote");
        tool.nodes[0].children.push(bad_child);
        assert!(ToolRuntime::new(vec![tool], Arc::new(FakeInvoker(count.clone()))).is_err());
    }
}

#[test]
fn redirection_descriptors_do_not_remove_spaced_numeric_arguments() {
    assert_eq!(words("remote read 2 >file 2>/err"), ["remote", "read", "2"]);
    assert_eq!(words("remote read 2>file"), ["remote", "read"]);
}
