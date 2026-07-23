//! Text/data builtins backed by Rust libraries.
//!
//! These are intentionally small POC wrappers over library APIs, registered as Brush builtins so
//! they run inside clank on both native and wasm. They focus on file-argument workflows for now;
//! stdin/pipeline fidelity needs the future process model instead of process-global fd swapping.

#![allow(clippy::similar_names)] // argv/args/arg-style locals are inherent to arg parsing here

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

            #[allow(clippy::cast_sign_loss)] // code is clamped to 0..=255 before the u8 cast
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
                let code = crate::tools::coreutils::run_tool(&context, move |stdin, out, err| {
                    match $run(&argv, stdin, out, err) {
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
text_builtin!(Sed, "sed", "stream editor (s///, d, p, q; line/regex addresses)", run_sed);
text_builtin!(Awk, "awk", "pattern scanning and text processing", crate::tools::awk::run_awk);
text_builtin!(Diff, "diff", "compare files line by line", run_diff);
text_builtin!(Patch, "patch", "apply a diff to a file", run_patch);
text_builtin!(File, "file", "identify file type", run_file);

pub(crate) fn builtins<SE: ShellExtensions>() -> Vec<(String, Registration<SE>)> {
    use brush_core::builtins::simple_builtin;
    vec![
        ("jq".into(), simple_builtin::<Jq, SE>()),
        ("grep".into(), simple_builtin::<Grep, SE>()),
        ("sed".into(), simple_builtin::<Sed, SE>()),
        ("awk".into(), simple_builtin::<Awk, SE>()),
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
        Manifest::builtin(Sed::NAME, Sed::SYNOPSIS).with_help(
            "sed [-n] [-e SCRIPT]... [SCRIPT] [FILE...] — stream editor. Commands: s/RE/REPL/ \
             (flags g, i, N), d, p, q; addresses: line numbers, $, /RE/, and addr1,addr2 ranges; \
             multiple commands via -e or ';'. -i (in-place) is not supported: redirect instead.",
        ),
        Manifest::builtin(Awk::NAME, Awk::SYNOPSIS).with_help(
            "awk [-F fs] [-v var=val] 'program' [FILE...] — pattern scanning subset: \
             pattern { action } rules with /regex/ and comparison patterns, BEGIN/END, fields \
             $0..$NF, NR NF FS OFS, user variables, arithmetic, string concat, ~ and !~, print \
             and printf, length(). Not supported: arrays, if/while/for, user functions, getline.",
        ),
        Manifest::builtin(Diff::NAME, Diff::SYNOPSIS),
        Manifest::builtin(Patch::NAME, Patch::SYNOPSIS),
        Manifest::builtin(File::NAME, File::SYNOPSIS),
    ]
}

#[allow(clippy::similar_names)] // files/file, values/value, inputs/input are conventional
fn run_jq(
    argv: &[String],
    stdin: &mut dyn std::io::Read,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ToolResult<i32> {
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
                let _ = writeln!(err, "jq: {e}");
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
                    let _ = writeln!(err, "jq: {e}");
                    failed = true;
                }
            }
        }
    }

    Ok(i32::from(failed))
}

/// Parsed `grep` invocation. Flags may appear anywhere (GNU permutes); short flags cluster
/// (`-in`); `--` ends flag parsing; `-e PATTERN` may repeat.
#[derive(Default)]
#[allow(clippy::struct_excessive_bools)] // one bool field per grep CLI flag
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

#[allow(clippy::similar_names)] // args/arg are conventional
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
    if file == "/dev/null" {
        // The emulated null device: reads as empty.
        Ok(Vec::new())
    } else if crate::runtime::binfs::is_bin_path(file) {
        crate::runtime::binfs::resolve(file)
            .map(String::into_bytes)
            .map_err(|_| format!("{file}: No such file or directory"))
    } else if crate::runtime::procfs::is_proc_path(file) {
        crate::runtime::proctable::active()
            .and_then(|t| {
                crate::runtime::procfs::resolve(file, &t.lock().unwrap(), environ).ok()
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
                    .filter_map(std::result::Result::ok)
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

#[allow(clippy::too_many_lines, clippy::similar_names)] // single search-dispatch fn; matched/matcher, files/file, roots/root conventional
fn run_grep(
    argv: &[String],
    stdin: &mut dyn std::io::Read,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ToolResult<i32> {
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
            let _ = writeln!(err, "grep: {e}");
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

    let environ = crate::runtime::procfs::current_environ();
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
                    let _ = writeln!(err, "grep: {e}");
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
                let _ = writeln!(err, "grep: {display}: {e}");
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
            let result = if let Some(file) = label {
                let mut sink = printer.sink_with_path(&matcher, file);
                let r = searcher.search_slice(&matcher, bytes, &mut sink);
                if sink.has_match() {
                    matched = true;
                }
                r
            } else {
                let mut sink = printer.sink(&matcher);
                let r = searcher.search_slice(&matcher, bytes, &mut sink);
                if sink.has_match() {
                    matched = true;
                }
                r
            };
            if let Err(e) = result {
                let _ = writeln!(err, "grep: {display}: {e}");
                failed = true;
            }
        }
    }

    Ok(if failed {
        2
    } else { i32::from(!matched) })
}

#[allow(clippy::similar_names)] // scripts/script, files/file are conventional
fn run_sed(
    argv: &[String],
    stdin: &mut dyn std::io::Read,
    out: &mut dyn Write,
    _err: &mut dyn Write,
) -> ToolResult<i32> {
    let args = &argv[1..];
    let mut suppress = false; // -n
    let mut scripts: Vec<String> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-n" | "--quiet" | "--silent" => suppress = true,
            "-e" | "--expression" => {
                let s = iter.next().ok_or("option requires an argument -- 'e'")?;
                scripts.push(s.clone());
            }
            "-i" | "--in-place" => {
                return Err("-i (in-place) is not supported; redirect to a new file instead".into());
            }
            f if f.starts_with('-') && f.len() > 1 => {
                return Err(format!("unknown option '{f}'").into());
            }
            operand => {
                if scripts.is_empty() {
                    scripts.push(operand.to_string());
                } else {
                    files.push(operand.to_string());
                }
            }
        }
    }
    if scripts.is_empty() {
        return usage("sed [-n] [-e SCRIPT]... [SCRIPT] [FILE...]");
    }

    let mut commands = Vec::new();
    for script in &scripts {
        commands.extend(sed::parse_script(script)?);
    }

    // GNU sed treats all inputs as one continuous stream: line numbers keep counting across
    // files and `$` addresses the very last line.
    let mut text = String::new();
    if files.is_empty() {
        stdin.read_to_string(&mut text)?;
    } else {
        for file in &files {
            text.push_str(&std::fs::read_to_string(file)?);
        }
    }

    sed::execute(commands, &text, suppress, out)?;
    Ok(0)
}

/// The sed mini-engine: line-oriented execution of `[addr[,addr]] cmd` scripts.
///
/// Supported: addresses `N`, `$`, `/re/` and ranges of them; commands `s///` (flags `g`, `i`,
/// occurrence number), `d`, `p`, `q`; `-n` auto-print suppression; multiple commands via `-e` or
/// `;`-separation. Everything else errors honestly. The engine is deliberately buffer-backed
/// (inputs are read whole) — `$` needs to know the last line, and clank inputs are small.
mod sed {
    use super::{read_delimited, ToolResult};

    pub(super) enum Addr {
        Line(usize),
        Last,
        Regex(regex::Regex),
    }

    pub(super) struct AddrRange {
        start: Addr,
        /// `None` = single-address match, `Some` = `addr1,addr2` range.
        end: Option<Addr>,
    }

    pub(super) enum Action {
        Subst {
            re: regex::Regex,
            replacement: String,
            global: bool,
            /// Replace only the Nth occurrence (s///2). Wins over `global` if both given.
            occurrence: Option<usize>,
        },
        Delete,
        Print,
        Quit,
    }

    pub(super) struct Command {
        addr: Option<AddrRange>,
        action: Action,
        /// Range activation state (`/a/,/b/` is stateful across lines).
        active: bool,
    }

    /// Parse one script string into commands (`;`/newline separated).
    pub(super) fn parse_script(script: &str) -> ToolResult<Vec<Command>> {
        let mut commands = Vec::new();
        let mut rest = script.trim_start();
        while !rest.is_empty() {
            // Separators between commands.
            if let Some(r) = rest.strip_prefix([';', '\n']) {
                rest = r.trim_start();
                continue;
            }
            let (addr, r) = parse_addr_range(rest)?;
            rest = r.trim_start();
            let Some(cmd_char) = rest.chars().next() else {
                return Err("missing command".into());
            };
            rest = &rest[cmd_char.len_utf8()..];
            let action = match cmd_char {
                's' => {
                    let Some(delim) = rest.chars().next() else {
                        return Err("missing substitution delimiter".into());
                    };
                    rest = &rest[delim.len_utf8()..];
                    let (pattern, r) = read_delimited(rest, delim)?;
                    let (replacement, r) = read_delimited(r, delim)?;
                    rest = r;
                    let mut global = false;
                    let mut ignore_case = false;
                    let mut occurrence: Option<usize> = None;
                    while let Some(c) = rest.chars().next() {
                        match c {
                            'g' => global = true,
                            'i' | 'I' => ignore_case = true,
                            '0'..='9' => {
                                let digits: String =
                                    rest.chars().take_while(char::is_ascii_digit).collect();
                                occurrence = Some(digits.parse()?);
                                rest = &rest[digits.len()..];
                                continue;
                            }
                            ';' | '\n' | ' ' => break,
                            other => return Err(format!("unknown s flag '{other}'").into()),
                        }
                        rest = &rest[c.len_utf8()..];
                    }
                    let pattern = if ignore_case {
                        format!("(?i){pattern}")
                    } else {
                        pattern
                    };
                    Action::Subst {
                        re: regex::Regex::new(&pattern)?,
                        replacement: convert_replacement(&replacement),
                        global,
                        occurrence,
                    }
                }
                'd' => Action::Delete,
                'p' => Action::Print,
                'q' => Action::Quit,
                other => {
                    return Err(format!(
                        "unsupported command '{other}' (supported: s, d, p, q)"
                    )
                    .into())
                }
            };
            commands.push(Command {
                addr,
                action,
                active: false,
            });
        }
        Ok(commands)
    }

    /// Optional leading `addr` or `addr1,addr2`.
    fn parse_addr_range(input: &str) -> ToolResult<(Option<AddrRange>, &str)> {
        let (Some(start), rest) = parse_addr(input)? else {
            return Ok((None, input));
        };
        if let Some(r) = rest.strip_prefix(',') {
            let (Some(end), rest) = parse_addr(r)? else {
                return Err("expected address after ','".into());
            };
            return Ok((
                Some(AddrRange {
                    start,
                    end: Some(end),
                }),
                rest,
            ));
        }
        Ok((Some(AddrRange { start, end: None }), rest))
    }

    #[allow(clippy::type_complexity)]
    fn parse_addr(input: &str) -> ToolResult<(Option<Addr>, &str)> {
        let mut chars = input.chars();
        match chars.next() {
            Some('$') => Ok((Some(Addr::Last), chars.as_str())),
            Some('/') => {
                let (pattern, rest) = read_delimited(chars.as_str(), '/')?;
                Ok((Some(Addr::Regex(regex::Regex::new(&pattern)?)), rest))
            }
            Some(c) if c.is_ascii_digit() => {
                let digits: String = input.chars().take_while(char::is_ascii_digit).collect();
                let n: usize = digits.parse()?;
                Ok((Some(Addr::Line(n)), &input[digits.len()..]))
            }
            _ => Ok((None, input)),
        }
    }

    /// sed replacement syntax → regex-crate replacement syntax:
    /// `&` → `$0`, `\1`..`\9` → `${1}`..`${9}`, `\&`/`\\` → literals, `$` escaped.
    fn convert_replacement(replacement: &str) -> String {
        let mut out = String::new();
        let mut chars = replacement.chars();
        while let Some(c) = chars.next() {
            match c {
                '&' => out.push_str("$0"),
                '$' => out.push_str("$$"),
                '\\' => match chars.next() {
                    Some(d @ '1'..='9') => {
                        out.push_str("${");
                        out.push(d);
                        out.push('}');
                    }
                    Some('&') => out.push('&'),
                    Some('\\') => out.push('\\'),
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some(other) => out.push(other),
                    None => {}
                },
                other => out.push(other),
            }
        }
        out
    }

    fn addr_matches(addr: &Addr, line_no: usize, is_last: bool, line: &str) -> bool {
        match addr {
            Addr::Line(n) => *n == line_no,
            Addr::Last => is_last,
            Addr::Regex(re) => re.is_match(line),
        }
    }

    /// Whether this command applies to the current line, updating range state.
    fn command_applies(cmd: &mut Command, line_no: usize, is_last: bool, line: &str) -> bool {
        let Some(range) = &cmd.addr else { return true };
        match &range.end {
            None => addr_matches(&range.start, line_no, is_last, line),
            Some(end) => {
                if cmd.active {
                    if addr_matches(end, line_no, is_last, line) {
                        cmd.active = false;
                    }
                    true
                } else if addr_matches(&range.start, line_no, is_last, line) {
                    // Activate; a numeric end already behind us makes it a one-line range.
                    cmd.active = true;
                    if let Addr::Line(n) = end {
                        if *n <= line_no {
                            cmd.active = false;
                        }
                    }
                    true
                } else {
                    false
                }
            }
        }
    }

    /// Replace only the Nth match (sed `s///N`).
    fn replace_nth(re: &regex::Regex, text: &str, replacement: &str, n: usize) -> String {
        let mut count = 0;
        let mut out = String::new();
        let mut last = 0;
        // Iterate `captures_iter` (not `find_iter` + a re-`captures` of the isolated match slice): the
        // captures are computed in FULL context, so a context-dependent zero-width assertion (`\b`,
        // `\B`, `^`, `$`, lookaround) evaluates correctly. Re-running `captures` on the isolated slice
        // could return `None` — the assertion flips at the slice edge (e.g. `\B` in `aX` vs `"X"`) —
        // and panic on `unwrap`, which on the wasm agent traps the durable instance (audit P0-2).
        for caps in re.captures_iter(text) {
            count += 1;
            if count == n {
                // Group 0 (the whole match) is always present for a `Captures` from `captures_iter`.
                let m = caps.get(0).expect("capture group 0 is always present in a match");
                out.push_str(&text[last..m.start()]);
                let mut expanded = String::new();
                caps.expand(replacement, &mut expanded);
                out.push_str(&expanded);
                last = m.end();
                break;
            }
        }
        out.push_str(&text[last..]);
        out
    }

    /// Run the commands over the input. Takes the commands by value — ranges carry activation
    /// state across lines, and each invocation parses its script fresh.
    pub(super) fn execute(
        mut commands: Vec<Command>,
        text: &str,
        suppress: bool,
        out: &mut dyn super::Write,
    ) -> ToolResult<()> {
        let lines: Vec<&str> = text.split_inclusive('\n').collect();
        let total = lines.len();

        'lines: for (idx, raw) in lines.iter().enumerate() {
            let line_no = idx + 1;
            let is_last = line_no == total;
            let had_newline = raw.ends_with('\n');
            let mut space = raw.strip_suffix('\n').unwrap_or(raw).to_string();
            let mut deleted = false;
            let mut quit = false;

            for cmd in &mut commands {
                if !command_applies(cmd, line_no, is_last, &space) {
                    continue;
                }
                match &cmd.action {
                    Action::Subst {
                        re,
                        replacement,
                        global,
                        occurrence,
                    } => {
                        space = if let Some(n) = occurrence {
                            replace_nth(re, &space, replacement, *n)
                        } else if *global {
                            re.replace_all(&space, replacement.as_str()).into_owned()
                        } else {
                            re.replace(&space, replacement.as_str()).into_owned()
                        };
                    }
                    Action::Delete => {
                        deleted = true;
                        break;
                    }
                    Action::Print => {
                        out.write_all(space.as_bytes())?;
                        out.write_all(b"\n")?;
                    }
                    Action::Quit => {
                        quit = true;
                        break;
                    }
                }
            }

            if !deleted && !suppress {
                out.write_all(space.as_bytes())?;
                if had_newline {
                    out.write_all(b"\n")?;
                }
            }
            if quit {
                break 'lines;
            }
        }
        Ok(())
    }
}

fn run_diff(
    argv: &[String],
    _stdin: &mut dyn std::io::Read,
    out: &mut dyn Write,
    _err: &mut dyn Write,
) -> ToolResult<i32> {
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
    Ok(i32::from(!rendered.is_empty()))
}

fn run_patch(
    argv: &[String],
    _stdin: &mut dyn std::io::Read,
    _out: &mut dyn Write,
    _err: &mut dyn Write,
) -> ToolResult<i32> {
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

fn run_file(
    argv: &[String],
    _stdin: &mut dyn std::io::Read,
    out: &mut dyn Write,
    _err: &mut dyn Write,
) -> ToolResult<i32> {
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
        if let Some(kind) = infer::get_from_path(path)? { writeln!(out, "{file}: {} ({})", kind.mime_type(), kind.extension())? } else {
            let bytes = std::fs::read(path)?;
            if std::str::from_utf8(&bytes).is_ok() {
                writeln!(out, "{file}: text/plain")?;
            } else {
                writeln!(out, "{file}: data")?;
            }
        }
    }
    Ok(0)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Run a sed script over input text, returning stdout.
    fn sed_run(args: &[&str], input: &str) -> String {
        let argv: Vec<String> = std::iter::once("sed")
            .chain(args.iter().copied())
            .map(String::from)
            .collect();
        let mut stdin = input.as_bytes();
        let mut out = Vec::new();
        let mut err = Vec::new();
        run_sed(&argv, &mut stdin, &mut out, &mut err).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn sed_substitution_first_global_and_nth() {
        assert_eq!(sed_run(&["s/a/X/"], "aa\naa\n"), "Xa\nXa\n");
        assert_eq!(sed_run(&["s/a/X/g"], "aa\naa\n"), "XX\nXX\n");
        assert_eq!(sed_run(&["s/a/X/2"], "aaa\n"), "aXa\n");
    }

    #[test]
    fn sed_nth_with_context_dependent_assertion_does_not_panic() {
        // Audit P0-2: `s/\BX/Y/1` re-ran captures on the isolated match slice, where `\B` (true in
        // `aX`) is false at the slice edge → `None` → panic (a durable-agent trap on wasm). The
        // captures-in-context fix must produce the substitution without panicking.
        assert_eq!(sed_run(&["s/\\BX/Y/1"], "aX\n"), "aY\n");
    }

    #[test]
    fn sed_replacement_backrefs_and_ampersand() {
        assert_eq!(sed_run(&["s/b/[&]/"], "abc\n"), "a[b]c\n");
        // ERE groups with sed-style \1 backrefs.
        assert_eq!(sed_run(&[r"s/(a+)b/[\1]/"], "aab\n"), "[aa]\n");
        // A literal $ in the replacement stays literal (not regex-crate capture syntax).
        assert_eq!(sed_run(&["s/(a+)b/<$x&>/"], "aab\n"), "<$xaab>\n");
    }

    #[test]
    fn sed_delete_and_print_with_addresses() {
        assert_eq!(sed_run(&["/^#/d"], "#c\nkeep\n#d\n"), "keep\n");
        assert_eq!(sed_run(&["1d"], "a\nb\nc\n"), "b\nc\n");
        assert_eq!(sed_run(&["$d"], "a\nb\nc\n"), "a\nb\n");
        assert_eq!(sed_run(&["-n", "2p"], "a\nb\nc\n"), "b\n");
        assert_eq!(sed_run(&["-n", "2,3p"], "a\nb\nc\nd\n"), "b\nc\n");
        assert_eq!(sed_run(&["-n", "/b/,/c/p"], "a\nb\nx\nc\nd\n"), "b\nx\nc\n");
    }

    #[test]
    fn sed_quit_and_multiple_commands() {
        assert_eq!(sed_run(&["2q"], "a\nb\nc\n"), "a\nb\n");
        assert_eq!(sed_run(&["-e", "s/a/1/", "-e", "s/b/2/"], "ab\n"), "12\n");
        assert_eq!(sed_run(&["s/a/1/;s/b/2/"], "ab\n"), "12\n");
    }

    #[test]
    fn sed_n_suppresses_autoprint_and_p_duplicates() {
        assert_eq!(sed_run(&["p"], "x\n"), "x\nx\n");
        assert_eq!(sed_run(&["-n", "p"], "x\n"), "x\n");
    }

    #[test]
    fn sed_unsupported_surface_is_honest() {
        let argv: Vec<String> = ["sed", "y/ab/cd/"].map(String::from).to_vec();
        let mut stdin = "x\n".as_bytes();
        let mut out = Vec::new();
        let mut errbuf = Vec::new();
        let err = run_sed(&argv, &mut stdin, &mut out, &mut errbuf).unwrap_err();
        assert!(err.to_string().contains("unsupported command"), "{err}");
    }
}
