//! The `find` builtin: recursive path search over the real filesystem and the virtual namespaces.
//!
//! Hand-written — uutils' findutils project is bin-only with C deps (`onig`) that don't build for
//! wasm32-wasip2, so this is a subset implementation of the predicates LLMs actually reach for:
//! `-name`/`-iname` (glob on the basename), `-path` (glob on the whole path), `-type f|d`,
//! `-maxdepth`/`-mindepth`, and the implicit `-print`. Predicates AND together, GNU-style.
//!
//! Virtual namespaces are served at listing depth, like `ls`: `find /bin` emits `/bin` and every
//! `/bin/<name>`; `find /proc/<pid>` and `find /proc/clank` emit their fixed children. The real
//! walk never descends *into* `/bin`//`proc` (their entries aren't on disk).
//!
//! `-exec` is deliberately absent in v1 — `find ... | xargs cmd` is the supported composition.

use std::io::Write;

use brush_core::builtins::{ContentOptions, ContentType, Registration, SimpleCommand};
use brush_core::commands::ExecutionContext;
use brush_core::extensions::ShellExtensions;
use brush_core::{Error, ExecutionResult};

use crate::manifest::Manifest;

pub(crate) struct Find;

impl Find {
    const NAME: &'static str = "find";
    const SYNOPSIS: &'static str = "search for files in a directory hierarchy";
}

/// Translate a shell glob (`*`, `?`, `[...]`) to an anchored regex. `-name` matches basenames
/// (which contain no `/`), so `*` mapping to `.*` is exact; for `-path`, GNU find's `*` crosses
/// `/` anyway (fnmatch without FNM_PATHNAME), so the same mapping is correct there too.
fn glob_to_regex(glob: &str, ignore_case: bool) -> Result<regex::Regex, regex::Error> {
    let mut re = String::from(if ignore_case { "(?i)^" } else { "^" });
    for c in glob.chars() {
        match c {
            '*' => re.push_str(".*"),
            '?' => re.push('.'),
            // Pass character classes through — glob `[abc]`/`[a-z]` is regex-compatible.
            '[' | ']' => re.push(c),
            c if r"\.+()|{}^$".contains(c) => {
                re.push('\\');
                re.push(c);
            }
            c => re.push(c),
        }
    }
    re.push('$');
    regex::Regex::new(&re)
}

#[derive(Default)]
struct FindOpts {
    paths: Vec<String>,
    name: Option<regex::Regex>,
    path_glob: Option<regex::Regex>,
    /// `Some('f')` or `Some('d')`.
    file_type: Option<char>,
    maxdepth: Option<usize>,
    mindepth: usize,
}

fn parse_find_args(args: &[String]) -> Result<FindOpts, String> {
    let mut opts = FindOpts::default();
    let mut iter = args.iter();
    let mut seen_predicate = false;
    while let Some(arg) = iter.next() {
        let mut value_of = |flag: &str| {
            iter.next()
                .cloned()
                .ok_or_else(|| format!("missing argument to '{flag}'"))
        };
        match arg.as_str() {
            "-name" | "-iname" => {
                seen_predicate = true;
                let glob = value_of(arg)?;
                opts.name = Some(
                    glob_to_regex(&glob, arg == "-iname")
                        .map_err(|e| format!("invalid glob '{glob}': {e}"))?,
                );
            }
            "-path" => {
                seen_predicate = true;
                let glob = value_of(arg)?;
                opts.path_glob = Some(
                    glob_to_regex(&glob, false)
                        .map_err(|e| format!("invalid glob '{glob}': {e}"))?,
                );
            }
            "-type" => {
                seen_predicate = true;
                match value_of(arg)?.as_str() {
                    "f" => opts.file_type = Some('f'),
                    "d" => opts.file_type = Some('d'),
                    other => return Err(format!("unsupported -type '{other}' (only f and d)")),
                }
            }
            "-maxdepth" => {
                seen_predicate = true;
                let n = value_of(arg)?;
                opts.maxdepth =
                    Some(n.parse().map_err(|_| format!("invalid -maxdepth '{n}'"))?);
            }
            "-mindepth" => {
                seen_predicate = true;
                let n = value_of(arg)?;
                opts.mindepth = n.parse().map_err(|_| format!("invalid -mindepth '{n}'"))?;
            }
            "-print" => seen_predicate = true, // the implicit default
            p if p.starts_with('-') || p.starts_with('!') || p.starts_with('(') => {
                return Err(format!("unknown predicate '{p}'"));
            }
            path => {
                if seen_predicate {
                    return Err(format!("paths must precede expression: '{path}'"));
                }
                opts.paths.push(path.to_string());
            }
        }
    }
    if opts.paths.is_empty() {
        opts.paths.push(".".to_string());
    }
    Ok(opts)
}

fn basename(path: &str) -> &str {
    path.trim_end_matches('/').rsplit('/').next().unwrap_or(path)
}

impl FindOpts {
    /// Whether a visited entry passes every predicate (depth is checked by the caller).
    fn matches(&self, path: &str, is_dir: bool) -> bool {
        if let Some(t) = self.file_type {
            if (t == 'f' && is_dir) || (t == 'd' && !is_dir) {
                return false;
            }
        }
        if let Some(re) = &self.name {
            if !re.is_match(basename(path)) {
                return false;
            }
        }
        if let Some(re) = &self.path_glob {
            if !re.is_match(path) {
                return false;
            }
        }
        true
    }
}

/// Depth-first walk of one real-filesystem root. Children are sorted for deterministic output.
fn walk(path: &str, depth: usize, opts: &FindOpts, out: &mut dyn Write, errs: &mut Vec<String>) {
    let md = match std::fs::symlink_metadata(path) {
        Ok(md) => md,
        Err(e) => {
            errs.push(match e.kind() {
                std::io::ErrorKind::NotFound => {
                    format!("'{path}': No such file or directory")
                }
                _ => format!("'{path}': {e}"),
            });
            return;
        }
    };
    let is_dir = md.is_dir();
    if depth >= opts.mindepth && opts.matches(path, is_dir) {
        let _ = writeln!(out, "{path}");
    }
    if is_dir && opts.maxdepth.is_none_or(|max| depth < max) {
        match std::fs::read_dir(path) {
            Ok(rd) => {
                let mut children: Vec<String> = rd
                    .filter_map(|e| e.ok())
                    .map(|e| e.path().to_string_lossy().into_owned())
                    .collect();
                children.sort();
                for child in children {
                    walk(&child, depth + 1, opts, out, errs);
                }
            }
            Err(e) => errs.push(format!("'{path}': {e}")),
        }
    }
}

/// Serve a virtual root (`/bin`, `/proc/<pid>`, `/proc/clank`) at listing depth: the root as a
/// directory plus its fixed children as files. Returns false if the root doesn't resolve.
fn walk_virtual(root: &str, opts: &FindOpts, out: &mut dyn Write) -> bool {
    let children = if crate::runtime::binfs::is_bin_path(root) {
        crate::runtime::binfs::list_children(root)
    } else {
        crate::runtime::procfs::list_children(root)
    };
    let Some(children) = children else {
        // Not a listable virtual dir; a resolvable leaf (e.g. /bin/curl) is served as a file.
        let resolves = crate::runtime::binfs::is_bin_path(root) && crate::runtime::binfs::resolve(root).is_ok();
        if resolves && opts.mindepth == 0 && opts.matches(root, false) {
            let _ = writeln!(out, "{root}");
        }
        return resolves;
    };
    let base = root.trim_end_matches('/');
    if opts.mindepth == 0 && opts.matches(base, true) {
        let _ = writeln!(out, "{base}");
    }
    if opts.maxdepth.is_none_or(|max| max >= 1) && opts.mindepth <= 1 {
        for child in children {
            let path = format!("{base}/{child}");
            if opts.matches(&path, false) {
                let _ = writeln!(out, "{path}");
            }
        }
    }
    true
}

impl SimpleCommand for Find {
    fn get_content(
        name: &str,
        content_type: ContentType,
        _options: &ContentOptions,
    ) -> Result<String, Error> {
        match content_type {
            ContentType::ShortDescription => Ok(format!("{name} - {}\n", Find::SYNOPSIS)),
            ContentType::ShortUsage => Ok(format!(
                "{name}: {name} [PATH...] [-name G] [-iname G] [-path G] [-type f|d] [-maxdepth N] [-mindepth N]\n"
            )),
            ContentType::DetailedHelp => Ok(format!(
                "{name} - {}\n\nSubset: -name/-iname/-path globs, -type f|d, -maxdepth/-mindepth, \
                 implicit -print. Predicates AND together. Use `find ... | xargs CMD` instead of \
                 -exec.\n",
                Find::SYNOPSIS
            )),
            ContentType::ManPage => brush_core::error::unimp("man page not yet implemented"),
        }
    }

    fn execute<SE, I, S>(context: ExecutionContext<'_, SE>, args: I) -> Result<ExecutionResult, Error>
    where
        SE: ShellExtensions,
        I: Iterator<Item = S>,
        S: AsRef<str>,
    {
        let argv: Vec<String> = args.skip(1).map(|s| s.as_ref().to_string()).collect();
        let opts = match parse_find_args(&argv) {
            Ok(opts) => opts,
            Err(e) => {
                let _ = writeln!(context.stderr(), "find: {e}");
                return Ok(ExecutionResult::new(2));
            }
        };

        let mut out = context.stdout();
        let mut errs = Vec::new();
        for root in &opts.paths {
            if crate::runtime::binfs::is_bin_path(root) || crate::runtime::procfs::is_proc_path(root) {
                if !walk_virtual(root, &opts, &mut out) {
                    errs.push(format!("'{root}': No such file or directory"));
                }
            } else {
                walk(root, 0, &opts, &mut out, &mut errs);
            }
        }
        let failed = !errs.is_empty();
        for e in errs {
            let _ = writeln!(context.stderr(), "find: {e}");
        }
        Ok(ExecutionResult::new(if failed { 1 } else { 0 }))
    }
}

pub(crate) fn builtins<SE: ShellExtensions>() -> Vec<(String, Registration<SE>)> {
    use crate::builtins::helpshim::simple_builtin_with_help;
    vec![("find".into(), simple_builtin_with_help::<Find, SE>())]
}

pub(crate) fn manifests() -> Vec<Manifest> {
    vec![Manifest::builtin(Find::NAME, Find::SYNOPSIS).with_help(
        "find [PATH...] [-name GLOB] [-iname GLOB] [-path GLOB] [-type f|d] [-maxdepth N] \
         [-mindepth N] — search for files in a directory hierarchy (implicit -print; predicates \
         AND together). Serves the virtual /bin and /proc namespaces at listing depth. -exec is \
         not supported: pipe into xargs instead (find . -name '*.txt' | xargs wc -l).",
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_translation_matches_shell_semantics() {
        let re = glob_to_regex("*.txt", false).unwrap();
        assert!(re.is_match("a.txt"));
        assert!(!re.is_match("a.txt.bak"));
        let re = glob_to_regex("f?le", false).unwrap();
        assert!(re.is_match("file"));
        assert!(!re.is_match("fiile"));
        let re = glob_to_regex("[ab]x", false).unwrap();
        assert!(re.is_match("ax"));
        assert!(!re.is_match("cx"));
        // Case-insensitive for -iname.
        let re = glob_to_regex("*.TXT", true).unwrap();
        assert!(re.is_match("a.txt"));
        // Regex metacharacters in the glob are literal.
        let re = glob_to_regex("a.b", false).unwrap();
        assert!(re.is_match("a.b"));
        assert!(!re.is_match("axb"));
    }

    #[test]
    fn parse_rejects_paths_after_predicates_and_unknown_predicates() {
        assert!(parse_find_args(&["-name".into(), "x".into(), "/tmp".into()]).is_err());
        assert!(parse_find_args(&["-newer".into(), "x".into()]).is_err());
        assert!(parse_find_args(&["-type".into(), "l".into()]).is_err());
        let opts = parse_find_args(&[]).unwrap();
        assert_eq!(opts.paths, vec!["."]);
    }

    #[test]
    fn walk_finds_by_name_type_and_depth() {
        let root = std::env::temp_dir().join(format!("clank-find-test-{}", std::process::id()));
        let sub = root.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(root.join("a.txt"), "x").unwrap();
        std::fs::write(sub.join("b.txt"), "x").unwrap();
        std::fs::write(sub.join("c.log"), "x").unwrap();
        let rootstr = root.to_string_lossy().into_owned();

        let run = |args: Vec<String>| {
            let opts = parse_find_args(&args).unwrap();
            let mut out = Vec::new();
            let mut errs = Vec::new();
            walk(&rootstr, 0, &opts, &mut out, &mut errs);
            assert!(errs.is_empty(), "{errs:?}");
            String::from_utf8(out).unwrap()
        };

        let all_txt = run(vec![rootstr.clone(), "-name".into(), "*.txt".into()]);
        assert!(all_txt.contains("a.txt") && all_txt.contains("b.txt"));
        assert!(!all_txt.contains("c.log"));

        let dirs_only = run(vec![rootstr.clone(), "-type".into(), "d".into()]);
        assert!(dirs_only.contains("sub"));
        assert!(!dirs_only.contains("a.txt"));

        let shallow = run(vec![
            rootstr.clone(),
            "-maxdepth".into(),
            "1".into(),
            "-name".into(),
            "*.txt".into(),
        ]);
        assert!(shallow.contains("a.txt"));
        assert!(!shallow.contains("b.txt"));

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn virtual_bin_root_lists_commands() {
        let opts = parse_find_args(&["/bin".into(), "-name".into(), "gre*".into()]).unwrap();
        let mut out = Vec::new();
        assert!(walk_virtual("/bin", &opts, &mut out));
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("/bin/grep"), "got: {text}");
        assert!(!text.contains("/bin/curl"));
    }
}
