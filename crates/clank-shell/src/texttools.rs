//! Text/data builtins backed by Rust libraries.
//!
//! These are intentionally small POC wrappers over library APIs, registered as Brush builtins so
//! they run inside clank on both native and wasm. They focus on file-argument workflows for now;
//! stdin/pipeline fidelity needs the future process model instead of process-global fd swapping.

use brush_core::builtins::{ContentOptions, ContentType, Registration, SimpleCommand};
use brush_core::commands::ExecutionContext;
use brush_core::extensions::ShellExtensions;
use brush_core::{Error, ExecutionResult};
use std::io::Write;
use std::path::Path;

type ToolResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

/// The `$synopsis` feeds both `get_content` and the command [`Manifest`](crate::manifest::Manifest)
/// in [`manifests`], defined once so they can't drift.
macro_rules! text_builtin {
    ($ty:ident, $name:literal, $synopsis:literal, $run:path) => {
        pub(crate) struct $ty;

        impl $ty {
            const NAME: &'static str = $name;
            const SYNOPSIS: &'static str = $synopsis;
        }

        impl SimpleCommand for $ty {
            fn get_content(
                name: &str,
                content_type: ContentType,
                _options: &ContentOptions,
            ) -> Result<String, Error> {
                match content_type {
                    ContentType::ShortDescription => Ok(format!("{name} - {}\n", $ty::SYNOPSIS)),
                    ContentType::ShortUsage => Ok(format!("{name}: {name} [args...]\n")),
                    ContentType::DetailedHelp => {
                        Ok(format!("{name} - {}\n\n(clank text/data builtin)\n", $ty::SYNOPSIS))
                    }
                    ContentType::ManPage => brush_core::error::unimp("man page not yet implemented"),
                }
            }

            fn execute<SE, I, S>(
                context: ExecutionContext<'_, SE>,
                args: I,
            ) -> Result<ExecutionResult, Error>
            where
                SE: ShellExtensions,
                I: Iterator<Item = S>,
                S: AsRef<str>,
            {
                let argv: Vec<String> = args.map(|s| s.as_ref().to_string()).collect();
                // Write through Brush's stdout/stderr sinks (captured on wasm), not io::stdout().
                // `stdin` is Brush's assigned input `OpenFile` — the upstream pipe stage's output when
                // this command is on the right-hand side of a `|` — so tools can read piped input.
                let code = crate::coreutils::run_tool(&context, move |stdin, out, err| {
                    match $run(&argv, stdin, out) {
                        Ok(code) => code,
                        Err(e) => {
                            let _ = writeln!(err, "{}: {e}", $name);
                            1
                        }
                    }
                });
                Ok(ExecutionResult::new(code.clamp(0, 255) as u8))
            }
        }
    };
}

text_builtin!(Jq, "jq", "filter and transform JSON", run_jq);
text_builtin!(Grep, "grep", "search files for a pattern", run_grep);
text_builtin!(Sed, "sed", "stream-edit text (s/// substitution)", run_sed);
text_builtin!(Diff, "diff", "compare files line by line", run_diff);
text_builtin!(Patch, "patch", "apply a diff to a file", run_patch);
text_builtin!(File, "file", "identify file type", run_file);

pub(crate) fn builtins<SE: ShellExtensions>() -> Vec<(String, Registration<SE>)> {
    use brush_core::builtins::simple_builtin;
    vec![
        ("jq".into(), simple_builtin::<Jq, SE>()),
        ("grep".into(), simple_builtin::<Grep, SE>()),
        ("sed".into(), simple_builtin::<Sed, SE>()),
        ("diff".into(), simple_builtin::<Diff, SE>()),
        ("patch".into(), simple_builtin::<Patch, SE>()),
        ("file".into(), simple_builtin::<File, SE>()),
    ]
}

/// The [`Manifest`](crate::manifest::Manifest) for each text/data builtin, from the same
/// `NAME`/`SYNOPSIS` the commands expose. Names must match [`builtins`] (registry drift-guard test).
pub(crate) fn manifests() -> Vec<crate::manifest::Manifest> {
    use crate::manifest::Manifest;
    vec![
        Manifest::builtin(Jq::NAME, Jq::SYNOPSIS),
        Manifest::builtin(Grep::NAME, Grep::SYNOPSIS),
        Manifest::builtin(Sed::NAME, Sed::SYNOPSIS),
        Manifest::builtin(Diff::NAME, Diff::SYNOPSIS),
        Manifest::builtin(Patch::NAME, Patch::SYNOPSIS),
        Manifest::builtin(File::NAME, File::SYNOPSIS),
    ]
}

fn run_jq(argv: &[String], stdin: &mut dyn std::io::Read, out: &mut dyn Write) -> ToolResult<i32> {
    use jaq_core::data::JustLut;
    use jaq_core::load::{import, Arena, File, Loader};
    use jaq_core::{compile::Compiler, unwrap_valr, Ctx, Vars};
    use jaq_json::{read, write, Val};

    let args = &argv[1..];
    let mut null_input = false;
    let mut i = 0;
    while args
        .get(i)
        .is_some_and(|arg| arg == "-n" || arg == "--null-input")
    {
        null_input = true;
        i += 1;
    }

    let Some(filter_src) = args.get(i) else {
        return usage("jq FILTER FILE... | jq -n FILTER");
    };
    let files = &args[i + 1..];
    // With no file operands (and not -n), jq reads its JSON input from stdin — the pipeline case.
    let read_stdin = files.is_empty() && !null_input;

    let arena = Arena::default();
    let loader = Loader::new(
        jaq_core::defs()
            .chain(jaq_std::defs())
            .chain(jaq_json::defs()),
    );
    let modules = loader
        .load(
            &arena,
            File {
                code: filter_src,
                path: (),
            },
        )
        .map_err(|errs| format!("failed to load filter: {errs:?}"))?;
    import(&modules, |_path| Err("file loading not supported".into()))
        .map_err(|errs| format!("failed to import filter: {errs:?}"))?;

    let funs = jaq_core::funs::<JustLut<Val>>()
        .chain(jaq_std::funs::<JustLut<Val>>())
        .chain(jaq_json::funs::<JustLut<Val>>());
    let filter = Compiler::default()
        .with_funs(funs)
        .compile(modules)
        .map_err(|errs| format!("failed to compile filter: {errs:?}"))?;
    let ctx = Ctx::<JustLut<Val>>::new(&filter.lut, Vars::new([]));
    let pp = write::Pp::default();

    let inputs: Box<dyn Iterator<Item = Result<Val, String>>> = if null_input {
        Box::new(std::iter::once(Ok(Val::Null)))
    } else if read_stdin {
        let mut bytes = Vec::new();
        stdin.read_to_end(&mut bytes)?;
        let values: Vec<Result<Val, String>> = read::parse_many(&bytes)
            .map(|value| value.map_err(|e| format!("(standard input): {e}")))
            .collect();
        Box::new(values.into_iter())
    } else {
        let mut values = Vec::new();
        for file in files {
            let bytes = std::fs::read(file)?;
            for value in read::parse_many(&bytes) {
                values.push(value.map_err(|e| format!("{file}: {e}")));
            }
        }
        Box::new(values.into_iter())
    };

    let mut failed = false;
    for input in inputs {
        let input = match input {
            Ok(v) => v,
            Err(e) => {
                eprintln!("jq: {e}");
                failed = true;
                continue;
            }
        };
        for value in filter.id.run((ctx.clone(), input)) {
            match unwrap_valr(value) {
                Ok(v) => {
                    write::write(&mut *out, &pp, 0, &v)?;
                    writeln!(out)?;
                }
                Err(e) => {
                    eprintln!("jq: {e}");
                    failed = true;
                }
            }
        }
    }

    Ok(if failed { 1 } else { 0 })
}

/// Parsed `grep` invocation. Flags may appear anywhere (GNU permutes); short flags cluster
/// (`-in`); `--` ends flag parsing; `-e PATTERN` may repeat.
#[derive(Default)]
struct GrepOpts {
    patterns: Vec<String>,
    files: Vec<String>,
    line_number: bool,
    ignore_case: bool,
    invert: bool,             // -v
    word: bool,               // -w
    line_match: bool,         // -x
    fixed: bool,              // -F
    quiet: bool,              // -q
    count: bool,              // -c
    files_with_matches: bool, // -l
    recursive: bool,          // -r / -R
    /// `-H` forces the filename prefix on, `-h` off; `None` = default (multiple inputs).
    filename: Option<bool>,
}

fn parse_grep_args(args: &[String]) -> ToolResult<GrepOpts> {
    let mut o = GrepOpts::default();
    let mut positional: Vec<String> = Vec::new();
    let mut no_more_flags = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if no_more_flags || !arg.starts_with('-') || arg == "-" {
            positional.push(arg.clone());
        } else if arg == "--" {
            no_more_flags = true;
        } else if arg == "-e" || arg == "--regexp" {
            let p = iter
                .next()
                .ok_or("option requires an argument -- 'e'")?;
            o.patterns.push(p.clone());
        } else if let Some(long) = arg.strip_prefix("--") {
            match long {
                "line-number" => o.line_number = true,
                "ignore-case" => o.ignore_case = true,
                "invert-match" => o.invert = true,
                "word-regexp" => o.word = true,
                "line-regexp" => o.line_match = true,
                "fixed-strings" => o.fixed = true,
                "extended-regexp" => {} // patterns are already ERE-ish (Rust regex)
                "quiet" | "silent" => o.quiet = true,
                "count" => o.count = true,
                "files-with-matches" => o.files_with_matches = true,
                "recursive" => o.recursive = true,
                "with-filename" => o.filename = Some(true),
                "no-filename" => o.filename = Some(false),
                other => return Err(format!("unrecognized option '--{other}'").into()),
            }
        } else {
            for c in arg[1..].chars() {
                match c {
                    'n' => o.line_number = true,
                    'i' => o.ignore_case = true,
                    'v' => o.invert = true,
                    'w' => o.word = true,
                    'x' => o.line_match = true,
                    'F' => o.fixed = true,
                    'E' => {} // patterns are already ERE-ish (Rust regex)
                    'q' => o.quiet = true,
                    'c' => o.count = true,
                    'l' => o.files_with_matches = true,
                    'r' | 'R' => o.recursive = true,
                    'H' => o.filename = Some(true),
                    'h' => o.filename = Some(false),
                    other => return Err(format!("invalid option -- '{other}'").into()),
                }
            }
        }
    }
    if o.patterns.is_empty() {
        if positional.is_empty() {
            return usage("grep [-invwxqclrEFhH] [-e PATTERN]... PATTERN [FILE...]");
        }
        o.patterns.push(positional.remove(0));
    }
    o.files = positional;
    Ok(o)
}

/// A [`grep::searcher::Sink`] that only counts selected lines — the engine for `-c`, `-l`, and
/// `-q`, where the standard printer's output isn't wanted. The searcher already applies
/// `invert_match`, so "selected" is correct for `-v` too.
struct CountSink {
    count: u64,
}

impl grep::searcher::Sink for CountSink {
    type Error = std::io::Error;

    fn matched(
        &mut self,
        _searcher: &grep::searcher::Searcher,
        _mat: &grep::searcher::SinkMatch<'_>,
    ) -> Result<bool, Self::Error> {
        self.count += 1;
        Ok(true)
    }
}

/// Resolve one named grep input to bytes: virtual `/bin` and `/proc` paths from their resolvers
/// (so `grep x /bin/curl` and `grep State /proc/1/status` both work), anything else from disk.
fn read_named_input(file: &str, environ: &[(String, String)]) -> Result<Vec<u8>, String> {
    if crate::binfs::is_bin_path(file) {
        crate::binfs::resolve(file)
            .map(String::into_bytes)
            .map_err(|_| format!("{file}: No such file or directory"))
    } else if crate::procfs::is_proc_path(file) {
        crate::proctable::active()
            .and_then(|t| {
                crate::procfs::resolve(file, &t.lock().unwrap(), environ).ok()
            })
            .map(String::into_bytes)
            .ok_or_else(|| format!("{file}: No such file or directory"))
    } else {
        std::fs::read(file).map_err(|e| format!("{file}: {e}"))
    }
}

/// Depth-first, sorted recursion over the real filesystem for `-r`. Virtual namespaces are not
/// walked (their entries aren't files on disk); errors are reported per path, not fatal.
fn collect_files_recursive(root: &str, acc: &mut Vec<String>, errs: &mut Vec<String>) {
    match std::fs::metadata(root) {
        Ok(md) if md.is_dir() => match std::fs::read_dir(root) {
            Ok(rd) => {
                let mut children: Vec<String> = rd
                    .filter_map(|e| e.ok())
                    .map(|e| e.path().to_string_lossy().into_owned())
                    .collect();
                children.sort();
                for child in children {
                    collect_files_recursive(&child, acc, errs);
                }
            }
            Err(e) => errs.push(format!("{root}: {e}")),
        },
        Ok(_) => acc.push(root.to_string()),
        Err(e) => errs.push(format!("{root}: {e}")),
    }
}

fn run_grep(argv: &[String], stdin: &mut dyn std::io::Read, out: &mut dyn Write) -> ToolResult<i32> {
    use grep::regex::RegexMatcherBuilder;
    use grep::searcher::{BinaryDetection, SearcherBuilder};

    let opts = parse_grep_args(&argv[1..])?;

    let mut failed = false;

    // Expand `-r` directory operands into their files (default operand: the current directory).
    let files: Vec<String> = if opts.recursive {
        let roots = if opts.files.is_empty() {
            vec![".".to_string()]
        } else {
            opts.files.clone()
        };
        let mut collected = Vec::new();
        let mut errs = Vec::new();
        for root in &roots {
            collect_files_recursive(root, &mut collected, &mut errs);
        }
        for e in errs {
            eprintln!("grep: {e}");
            failed = true;
        }
        collected
    } else {
        opts.files.clone()
    };

    // With no file operands, grep reads standard input — the upstream stage of a pipeline. This is
    // the `cmd | grep PATTERN` path; the label "(standard input)" mirrors GNU grep's stdin naming.
    let read_stdin = files.is_empty();
    let with_filename = opts.filename.unwrap_or(files.len() > 1 || opts.recursive);

    // `build_many` treats each pattern per the builder config (alternation of regexes, or literal
    // strings under `-F` — joining with `|` by hand would corrupt fixed strings).
    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(opts.ignore_case)
        .word(opts.word)
        .whole_line(opts.line_match)
        .fixed_strings(opts.fixed)
        .build_many(&opts.patterns)?;
    let mut searcher = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .line_number(opts.line_number)
        .invert_match(opts.invert)
        .build();

    let environ = crate::procfs::current_environ();
    let mut matched = false;

    // The (label, bytes) inputs: stdin as one unnamed input, or each named operand.
    let inputs: Vec<(Option<String>, Vec<u8>)> = if read_stdin {
        let mut bytes = Vec::new();
        stdin.read_to_end(&mut bytes)?;
        vec![(None, bytes)]
    } else {
        let mut inputs = Vec::new();
        for file in &files {
            match read_named_input(file, &environ) {
                Ok(bytes) => inputs.push((Some(file.clone()), bytes)),
                Err(e) => {
                    eprintln!("grep: {e}");
                    failed = true;
                }
            }
        }
        inputs
    };

    if opts.quiet || opts.count || opts.files_with_matches {
        // Count-only modes: no printed match lines, so no printer (which would hold `out`).
        for (label, bytes) in &inputs {
            let display = label.as_deref().unwrap_or("(standard input)");
            let mut sink = CountSink { count: 0 };
            if let Err(e) = searcher.search_slice(&matcher, bytes, &mut sink) {
                eprintln!("grep: {display}: {e}");
                failed = true;
                continue;
            }
            if sink.count > 0 {
                matched = true;
            }
            if opts.count {
                if with_filename {
                    writeln!(out, "{display}:{}", sink.count)?;
                } else {
                    writeln!(out, "{}", sink.count)?;
                }
            } else if opts.files_with_matches && sink.count > 0 {
                writeln!(out, "{display}")?;
            }
        }
    } else {
        let mut printer_builder = grep::printer::StandardBuilder::new();
        printer_builder.path(with_filename);
        let mut printer = printer_builder.build_no_color(&mut *out);
        for (label, bytes) in &inputs {
            let display = label.as_deref().unwrap_or("(standard input)");
            let result = match label {
                Some(file) => {
                    let mut sink = printer.sink_with_path(&matcher, file);
                    let r = searcher.search_slice(&matcher, bytes, &mut sink);
                    if sink.has_match() {
                        matched = true;
                    }
                    r
                }
                None => {
                    let mut sink = printer.sink(&matcher);
                    let r = searcher.search_slice(&matcher, bytes, &mut sink);
                    if sink.has_match() {
                        matched = true;
                    }
                    r
                }
            };
            if let Err(e) = result {
                eprintln!("grep: {display}: {e}");
                failed = true;
            }
        }
    }

    Ok(if failed {
        2
    } else if matched {
        0
    } else {
        1
    })
}

fn run_sed(argv: &[String], stdin: &mut dyn std::io::Read, out: &mut dyn Write) -> ToolResult<i32> {
    let args = &argv[1..];
    let Some(script) = args.first() else {
        return usage("sed 's/PATTERN/REPLACEMENT/[g]' FILE...");
    };
    let files = &args[1..];
    let substitution = Substitution::parse(script)?;
    let regex = regex::Regex::new(&substitution.pattern)?;

    let apply = |text: &str, out: &mut dyn Write| -> ToolResult<()> {
        let replaced = if substitution.global {
            regex.replace_all(text, substitution.replacement.as_str())
        } else {
            regex.replace(text, substitution.replacement.as_str())
        };
        out.write_all(replaced.as_bytes())?;
        Ok(())
    };

    // With no file operands, sed edits its standard input — the pipeline case (`cmd | sed …`).
    if files.is_empty() {
        let mut text = String::new();
        stdin.read_to_string(&mut text)?;
        apply(&text, out)?;
    } else {
        for file in files {
            let text = std::fs::read_to_string(file)?;
            apply(&text, out)?;
        }
    }
    Ok(0)
}

fn run_diff(argv: &[String], _stdin: &mut dyn std::io::Read, out: &mut dyn Write) -> ToolResult<i32> {
    let args = &argv[1..];
    if args.len() != 2 {
        return usage("diff OLD NEW");
    }
    let old = std::fs::read_to_string(&args[0])?;
    let new = std::fs::read_to_string(&args[1])?;
    let diff = similar::TextDiff::from_lines(&old, &new);
    let rendered = diff
        .unified_diff()
        .context_radius(3)
        .header(&args[0], &args[1])
        .to_string();
    write!(out, "{rendered}")?;
    Ok(if rendered.is_empty() { 0 } else { 1 })
}

fn run_patch(argv: &[String], _stdin: &mut dyn std::io::Read, _out: &mut dyn Write) -> ToolResult<i32> {
    let args = &argv[1..];
    if args.len() != 2 {
        return usage("patch FILE PATCHFILE");
    }
    let original = std::fs::read_to_string(&args[0])?;
    let patch_text = std::fs::read_to_string(&args[1])?;
    let patch = diffy::Patch::from_str(&patch_text)?;
    let modified = diffy::apply(&original, &patch)?;
    std::fs::write(&args[0], modified)?;
    Ok(0)
}

fn run_file(argv: &[String], _stdin: &mut dyn std::io::Read, out: &mut dyn Write) -> ToolResult<i32> {
    let args = &argv[1..];
    if args.is_empty() {
        return usage("file FILE...");
    }

    for file in args {
        let path = Path::new(file);
        if path.is_dir() {
            writeln!(out, "{file}: directory")?;
            continue;
        }
        match infer::get_from_path(path)? {
            Some(kind) => writeln!(out, "{file}: {} ({})", kind.mime_type(), kind.extension())?,
            None => {
                let bytes = std::fs::read(path)?;
                if std::str::from_utf8(&bytes).is_ok() {
                    writeln!(out, "{file}: text/plain")?;
                } else {
                    writeln!(out, "{file}: data")?;
                }
            }
        }
    }
    Ok(0)
}

struct Substitution {
    pattern: String,
    replacement: String,
    global: bool,
}

impl Substitution {
    fn parse(script: &str) -> ToolResult<Self> {
        let mut chars = script.chars();
        if chars.next() != Some('s') {
            return Err("only s/PATTERN/REPLACEMENT/[g] is supported".into());
        }
        let Some(delim) = chars.next() else {
            return Err("missing substitution delimiter".into());
        };
        let rest: String = chars.collect();
        let (pattern, rest) = read_delimited(&rest, delim)?;
        let (replacement, flags) = read_delimited(rest, delim)?;
        Ok(Self {
            pattern,
            replacement,
            global: flags.contains('g'),
        })
    }
}

fn read_delimited(input: &str, delim: char) -> ToolResult<(String, &str)> {
    let mut out = String::new();
    let mut escaped = false;
    for (idx, ch) in input.char_indices() {
        if escaped {
            out.push(ch);
            escaped = false;
        } else if ch == '\\' {
            out.push(ch);
            escaped = true;
        } else if ch == delim {
            return Ok((out, &input[idx + ch.len_utf8()..]));
        } else {
            out.push(ch);
        }
    }
    Err("unterminated substitution".into())
}

fn usage<T>(message: &'static str) -> ToolResult<T> {
    Err(format!("usage: {message}").into())
}
