//! Round-trip CLI arguments through the authoritative fixture schemas.
use bash::agent_tools::{parse, Projection, ToolRequest};
use golem_rust::schema::tool::Tool;
use serde_json::{json, Value};

fn fixture(name: &str) -> Tool {
    let value: Value = serde_json::from_str(include_str!(
        "../../../fixtures/echo-tool/tools.snapshot.json"
    ))
    .unwrap();
    serde_json::from_value(value[name].clone()).unwrap()
}
fn input(argv: &[&str]) -> (Tool, ToolRequest) {
    let tool = fixture("echo-tool");
    let definition = bash_golem::project("echo-tool".into(), &tool).unwrap();
    let Projection::Call { command, input } = parse(
        &definition,
        &argv.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        |_| None,
    )
    .unwrap() else {
        panic!("unexpected help")
    };
    let request = ToolRequest {
        name: "echo-tool".into(),
        path: command.path,
        input,
        stdin: None,
    };
    bash_golem::encode_input(&tool, &request).unwrap();
    (tool, request)
}
#[test]
fn inherited_globals_defaults_aliases_and_short_bundles_round_trip() {
    let (_, request) = input(&["greet", "-vv", "Ada", "-n3", "--shout", "--color", "always"]);
    assert_eq!(
        request.input,
        json!({"verbose":2,"color":"always","name":"Ada","times":3,"shout":true})
    );
    let (_, request) = input(&["greet", "Ada", "--no-shout"]);
    assert_eq!(request.input["times"], 1);
    assert_eq!(request.input["shout"], false);
    assert_eq!(request.input["verbose"], 0);
}
#[test]
fn metadata_help_and_usage_errors_do_not_invoke() {
    let definition = bash_golem::project("bound-alias".into(), &fixture("echo-tool")).unwrap();
    assert!(
        matches!(parse(&definition, &["tree".into(),"add".into(),"--help".into()], |_|None).unwrap(), Projection::Help(text) if text.contains("Usage: bound-alias tree add") && text.contains("entries"))
    );
    for words in [
        &["greet", "Ada", "--times", "4294967296"][..],
        &["greet"][..],
        &["greet", "Ada", "--bogus"][..],
    ] {
        assert!(parse(
            &definition,
            &words.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            |_| None
        )
        .is_err());
    }
}
#[test]
fn repeatable_maps_records_and_presence_constraints_round_trip() {
    let (_, request) = input(&[
        "tree",
        "add",
        "--entries",
        "a=1",
        "--entries",
        "b=2",
        "--meta",
        r#"{"k":"x","n":2}"#,
    ]);
    assert_eq!(request.input["entries"], json!([["a", "1"], ["b", "2"]]));
    let definition = bash_golem::project("echo-tool".into(), &fixture("echo-tool")).unwrap();
    assert!(parse(
        &definition,
        &[
            "tree".into(),
            "add".into(),
            "--entries".into(),
            "a=1".into()
        ],
        |_| None
    )
    .is_err());
}
