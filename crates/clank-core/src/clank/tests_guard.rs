//! The shell core must not name a command family. Every file headed for the `bash` crate is
//! scanned; after the crate move the compiler enforces this and the test is deleted.

/// Directories whose every `.rs` file is shell core.
const CORE_DIRS: &[&str] = &["session", "builtins", "runtime", "tools"];

/// Top-level shell-core files. (`lib.rs` is here for its own body; the `pub mod ai;`-style
/// declarations don't name a family *path*, so they are not offences.)
const CORE_FILES: &[&str] = &[
    "authz.rs",
    "config.rs",
    "error.rs",
    "helpshim.rs",
    "lib.rs",
    "logging.rs",
    "manifest.rs",
    "plugin.rs",
    "registry.rs",
];

/// A reference to one of the command families is exactly a path into its module.
const FAMILY_PATHS: &[&str] = &[
    "crate::ai",
    "crate::mcp",
    "crate::grease",
    "crate::golem",
    "crate::clank",
];

/// `session/tests/` fixtures that build the families' fakes (a scripted `AskProvider`, an
/// `McpHttp`, a `GolemCluster`) or reach into the installed `Clank` to assert on its state. They
/// are the plug-in's tests living in the core's directory; Task 10 moves them out with it.
const EXEMPT_TEST_FILES: &[&str] = &[
    "mod.rs",
    "ask.rs",
    "grease.rs",
    "mcp.rs",
    "agent.rs",
    "resolution.rs",
];

/// Marker comments bracketing the two places in `session/mod.rs` that still name clank: the
/// `Session::new` construction and the provider-injection / REPL forwarders. Both are the
/// embedder's job from Task 10 on, and the markers leave with them.
const EXEMPT_BEGIN: &str = "GUARD-EXEMPT-BEGIN";
const EXEMPT_END: &str = "GUARD-EXEMPT-END";

/// Every `.rs` file under `dir`, recursively.
fn rust_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Whether `file` is one of the `session/tests/` fixtures exempt until Task 10.
fn is_exempt_test_file(file: &std::path::Path) -> bool {
    let in_tests_dir = file
        .parent()
        .and_then(|p| p.file_name())
        .is_some_and(|d| d == "tests");
    let name = file.file_name().unwrap_or_default();
    in_tests_dir && EXEMPT_TEST_FILES.iter().any(|f| name == *f)
}

#[test]
fn core_files_name_no_command_family() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files: Vec<std::path::PathBuf> = CORE_FILES.iter().map(|f| src.join(f)).collect();
    for dir in CORE_DIRS {
        rust_files(&src.join(dir), &mut files);
    }
    let mut offences = Vec::new();
    for file in files {
        if is_exempt_test_file(&file) {
            continue;
        }
        let text = std::fs::read_to_string(&file).unwrap();
        let mut exempt = false;
        for (n, line) in text.lines().enumerate() {
            if line.contains(EXEMPT_BEGIN) {
                exempt = true;
                continue;
            }
            if line.contains(EXEMPT_END) {
                exempt = false;
                continue;
            }
            if exempt || line.trim_start().starts_with("//") {
                continue;
            }
            if FAMILY_PATHS.iter().any(|p| line.contains(p)) {
                offences.push(format!("{}:{}: {}", file.display(), n + 1, line.trim()));
            }
        }
    }
    assert!(
        offences.is_empty(),
        "core files name a family:\n{}",
        offences.join("\n")
    );
}
