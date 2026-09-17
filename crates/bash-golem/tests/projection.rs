//! Round-trip CLI arguments through the authoritative fixture schemas.
use bash::agent_tools::{parse, parse_with_stdin, Projection, ToolRequest};
use golem_rust::schema::tool::Tool;
use serde_json::{json, Value};

// Helpers intentionally panic when the checked-in fixture or a round trip is invalid.
#[allow(clippy::unwrap_used)]
fn fixture(name: &str) -> Tool {
    let value: Value = serde_json::from_str(include_str!(
        "../../../fixtures/echo-tool/tools.snapshot.json"
    ))
    .unwrap();
    serde_json::from_value(value[name].clone()).unwrap()
}
#[allow(clippy::unwrap_used)]
fn input(argv: &[&str]) -> (Tool, ToolRequest) {
    let tool = fixture("echo-tool");
    let definition = bash_golem::project("echo-tool".into(), &tool).unwrap();
    let Projection::Call { command, input } = parse(
        &definition,
        &argv.iter().map(ToString::to_string).collect::<Vec<_>>(),
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
            &words.iter().map(ToString::to_string).collect::<Vec<_>>(),
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

fn projected_input(
    tool: &Tool,
    argv: &[&str],
    env: impl Fn(&str) -> Option<String>,
    stdin: Option<&[u8]>,
) -> Result<Value, String> {
    let definition = bash_golem::project("echo-tool".into(), tool)?;
    let Projection::Call { command, input } = parse_with_stdin(
        &definition,
        &argv.iter().map(ToString::to_string).collect::<Vec<_>>(),
        env,
        stdin,
    )?
    else {
        return Err("stdin or help instead of complete canonical input".into());
    };
    bash_golem::encode_input(
        tool,
        &ToolRequest {
            name: "echo-tool".into(),
            path: command.path,
            input: input.clone(),
            stdin: None,
        },
    )
    .map_err(|e| e.message)?;
    Ok(input)
}

#[test]
fn environment_flags_use_their_values_and_cli_takes_precedence() {
    let tool = fixture("echo-tool");
    let env = |name: &str| match name {
        "ECHO_VERBOSE" => Some("3".into()),
        "ECHO_SHOUT" => Some("yes".into()),
        _ => None,
    };
    let input = projected_input(&tool, &["greet", "Ada"], env, None).unwrap();
    assert_eq!(input["verbose"], 3);
    assert_eq!(input["shout"], true);
    let input = projected_input(&tool, &["greet", "Ada", "-v", "--no-shout"], env, None).unwrap();
    assert_eq!(input["verbose"], 1);
    assert_eq!(input["shout"], false);
    for (name, value) in [
        ("ECHO_VERBOSE", "4"),
        ("ECHO_VERBOSE", "invalid"),
        ("ECHO_SHOUT", "invalid"),
    ] {
        assert!(projected_input(
            &tool,
            &["greet", "Ada"],
            |key| (key == name).then(|| value.into()),
            None
        )
        .is_err());
    }
}

#[test]
fn stdin_positionals_wait_for_input_then_round_trip_exact_bytes() {
    let tool = fixture("echo-tool");
    let definition = bash_golem::project("echo-tool".into(), &tool).unwrap();
    assert!(matches!(
        parse(&definition, &["stdin-arg".into()], |_| None).unwrap(),
        Projection::Stdin { .. }
    ));
    for argv in [&["stdin-arg"][..], &["stdin-arg", "-"][..]] {
        let input = projected_input(&tool, argv, |_| None, Some(b"one\ntwo\n")).unwrap();
        assert_eq!(input["text"], "one\ntwo\n");
        assert!(projected_input(&tool, argv, |_| None, Some(&[255])).is_err());
    }
    assert_eq!(
        projected_input(&tool, &["stdin-arg", "literal"], |_| None, Some(b"ignored")).unwrap()
            ["text"],
        "literal"
    );
}

#[test]
fn stdin_typed_scalars_trim_whitespace_and_defaults_do_not_consume_stdin() {
    let mut raw = serde_json::to_value(fixture("echo-tool")).unwrap();
    let nodes = raw["commands"]["nodes"].as_array_mut().unwrap();
    let node = nodes.iter_mut().find(|n| n["name"] == "stdin-arg").unwrap();
    let argument = &mut node["body"]["positionals"]["fixed"][0];
    argument["type_"] = json!({"kind":"u32", "value":{}});
    let tool: Tool = serde_json::from_value(raw.clone()).unwrap();
    assert_eq!(
        projected_input(&tool, &["stdin-arg"], |_| None, Some(b" 42\n")).unwrap()["text"],
        42
    );
    assert!(projected_input(&tool, &["stdin-arg"], |_| None, Some(b"wrong\n")).is_err());
    let nodes = raw["commands"]["nodes"].as_array_mut().unwrap();
    let node = nodes.iter_mut().find(|n| n["name"] == "stdin-arg").unwrap();
    let argument = &mut node["body"]["positionals"]["fixed"][0];
    argument["required"] = false.into();
    argument["default"] = json!({"kind":"u32", "value":7});
    let tool: Tool = serde_json::from_value(raw).unwrap();
    assert_eq!(
        projected_input(&tool, &["stdin-arg"], |_| None, None).unwrap()["text"],
        7
    );
    assert_eq!(
        projected_input(&tool, &["stdin-arg", "-"], |_| None, Some(b"42\n")).unwrap()["text"],
        42
    );
}

#[test]
fn stdin_tails_enforce_cardinality_after_reading_and_separators_respect_verbatim() {
    let mut raw = serde_json::to_value(fixture("echo-tool")).unwrap();
    let tail = &mut raw["commands"]["nodes"][2]["body"]["positionals"]["tail"];
    tail["accepts_stdio"] = true.into();
    tail["min"] = 1.into();
    tail["max"] = 3.into();
    tail["separator"] = "::".into();
    tail["verbatim"] = false.into();
    let tool: Tool = serde_json::from_value(raw.clone()).unwrap();
    let input = projected_input(&tool, &["grep"], |_| None, Some(b"a\nb\n")).unwrap();
    assert_eq!(input["files"], json!(["a", "b"]));
    let input = projected_input(
        &tool,
        &["grep", "::", "first", "-", "last", "-v"],
        |_| None,
        Some(b"middle\n"),
    )
    .unwrap();
    assert_eq!(input["files"], json!(["first", "middle", "last"]));
    assert_eq!(input["verbose"], 1);
    for bytes in [&b""[..], &b"a\nb\nc\nd\n"[..]] {
        assert!(projected_input(&tool, &["grep"], |_| None, Some(bytes)).is_err());
    }
    raw["commands"]["nodes"][2]["body"]["positionals"]["tail"]["verbatim"] = true.into();
    let tool: Tool = serde_json::from_value(raw).unwrap();
    let input = projected_input(&tool, &["grep", "::", "file", "-v"], |_| None, None).unwrap();
    assert_eq!(input["files"], json!(["file", "-v"]));
    assert_eq!(input["verbose"], 0);
}

#[test]
fn constraints_referencing_aliases_resolve_to_canonical_fields() {
    let mut raw = serde_json::to_value(fixture("echo-tool")).unwrap();
    let body = &mut raw["commands"]["nodes"][2]["body"];
    body["options"][0]["aliases"] = json!(["expression"]);
    body["constraints"][0]["value"][1]["value"] = "expression".into();
    let tool: Tool = serde_json::from_value(raw).unwrap();
    assert!(projected_input(&tool, &["grep", "--expression", "a"], |_| None, None).is_err());
    let input = projected_input(
        &tool,
        &["grep", "--expression", "a,b", "--all-match"],
        |_| None,
        None,
    )
    .unwrap();
    assert_eq!(input["pattern"], json!(["a", "b"]));
    let mut raw = serde_json::to_value(fixture("echo-tool")).unwrap();
    let body = &mut raw["commands"]["nodes"][1]["body"];
    body["options"][0]["aliases"] = json!(["repeat"]);
    body["constraints"] = json!([{"kind":"requires-all", "value":[{"kind":"value-is", "value":{"name":"repeat", "value":{"kind":"u32", "value":2}}}]}]);
    let tool: Tool = serde_json::from_value(raw).unwrap();
    assert!(projected_input(&tool, &["greet", "Ada"], |_| None, None).is_err());
    assert_eq!(
        projected_input(&tool, &["greet", "Ada", "--repeat", "2"], |_| None, None).unwrap()
            ["times"],
        2
    );
}
