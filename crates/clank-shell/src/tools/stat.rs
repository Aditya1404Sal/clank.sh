//! The `stat` builtin: file status from `std::fs` metadata — the honest wasm subset.
//!
//! wasip2 has no `stat(2)` struct: no inode, uid/gid, mode bits, or block counts. Rather than
//! invent them, the Linux-shaped output prints `-` for every field the sandbox cannot know and
//! real values for the ones it can (size, type, mtime/atime, birth where the host supports it).
//! `uu_stat` was rejected for this job: its formatting core is `MetadataExt`-unix throughout.
//!
//! Like `cat`/`ls`, the virtual namespaces are first-class: `stat /bin/<name>` and
//! `stat /proc/...` report virtual read-only entries with the resolved content's size, so the
//! virtual fs never leaks "No such file" for paths other tools serve. (The README's MCP resource
//! metadata story later lands here too — `stat` on a mounted resource reflecting MCP annotations.)
//!
//! `-c FORMAT` supports the common directives (`%n %s %F %y %Y %x %X %w %W %%`) so scripted
//! `stat -c %s file` works; unknown directives print `?` like GNU stat.

use std::io::Write;
use std::time::SystemTime;

use brush_core::builtins::{ContentOptions, ContentType, Registration, SimpleCommand};
use brush_core::commands::ExecutionContext;
use brush_core::extensions::ShellExtensions;
use brush_core::{Error, ExecutionResult};

use crate::manifest::Manifest;

pub(crate) struct Stat;

impl Stat {
    const NAME: &'static str = "stat";
    const SYNOPSIS: &'static str = "display file status";
}

/// What we can honestly know about one path.
#[derive(Debug)]
struct StatInfo {
    path: String,
    size: u64,
    /// Human-readable file type, GNU-style (`regular file`, `directory`, `symbolic link`) plus
    /// clank's `virtual read-only file` for `/bin`//`proc` entries.
    kind: &'static str,
    modified: Option<SystemTime>,
    accessed: Option<SystemTime>,
    created: Option<SystemTime>,
}

/// Resolve one operand to a [`StatInfo`], serving the virtual namespaces before the real fs.
/// `follow` selects `metadata` (follow symlinks, `-L`) over the default `symlink_metadata`.
fn resolve(path: &str, follow: bool) -> Result<StatInfo, String> {
    let not_found = || format!("cannot stat '{path}': No such file or directory");

    if path == "/dev/null" {
        // The emulated null device (redirects handle it in the shell layer; not a real fs entry).
        return Ok(StatInfo {
            path: path.to_string(),
            size: 0,
            kind: "character special file",
            modified: None,
            accessed: None,
            created: None,
        });
    }
    if crate::runtime::binfs::is_bin_path(path) {
        if crate::runtime::binfs::list_children(path).is_some() {
            return Ok(StatInfo::virtual_dir(path));
        }
        let content = crate::runtime::binfs::resolve(path).map_err(|_| not_found())?;
        return Ok(StatInfo::virtual_file(path, content.len() as u64));
    }
    if crate::runtime::procfs::is_proc_path(path) {
        if crate::runtime::procfs::list_children(path).is_some() {
            return Ok(StatInfo::virtual_dir(path));
        }
        let environ = crate::runtime::procfs::current_environ();
        let content = crate::runtime::proctable::active()
            .map(|t| crate::runtime::procfs::resolve(path, &t.lock().unwrap(), &environ))
            .and_then(Result::ok)
            .ok_or_else(not_found)?;
        return Ok(StatInfo::virtual_file(path, content.len() as u64));
    }
    // `/mnt/mcp/...`: a mounted MCP resource. Static resources are real files (fall through to the fs);
    // directories, dynamic resources, and template stubs are virtual — serve them from the index so
    // `stat` reflects MCP metadata (README:784) even when there's no real file on disk.
    if crate::runtime::mcpfs::is_mcp_path(path) {
        if let Some(index) = crate::runtime::mcpfs::active() {
            match crate::runtime::mcpfs::classify(path, &index) {
                crate::runtime::mcpfs::McpPathKind::Directory(_) => return Ok(StatInfo::virtual_dir(path)),
                crate::runtime::mcpfs::McpPathKind::Dynamic { .. } | crate::runtime::mcpfs::McpPathKind::Template => {
                    return Ok(StatInfo::virtual_file(path, 0));
                }
                // Static → a real file on disk; fall through. NotFound → fall through (→ "No such file").
                crate::runtime::mcpfs::McpPathKind::Static | crate::runtime::mcpfs::McpPathKind::NotFound => {}
            }
        }
    }

    let md = if follow {
        std::fs::metadata(path)
    } else {
        std::fs::symlink_metadata(path)
    }
    .map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => not_found(),
        _ => format!("cannot stat '{path}': {e}"),
    })?;
    let ft = md.file_type();
    let kind = if ft.is_dir() {
        "directory"
    } else if ft.is_symlink() {
        "symbolic link"
    } else {
        "regular file"
    };
    Ok(StatInfo {
        path: path.to_string(),
        size: md.len(),
        kind,
        modified: md.modified().ok(),
        accessed: md.accessed().ok(),
        created: md.created().ok(),
    })
}

impl StatInfo {
    fn virtual_file(path: &str, size: u64) -> Self {
        Self {
            path: path.to_string(),
            size,
            kind: "virtual read-only file",
            modified: None,
            accessed: None,
            created: None,
        }
    }

    fn virtual_dir(path: &str) -> Self {
        Self {
            path: path.to_string(),
            size: 0,
            kind: "directory",
            modified: None,
            accessed: None,
            created: None,
        }
    }
}

/// `2026-07-10 09:12:33.123456789 +0000` (GNU stat's human timestamp; the agent runs in UTC), or
/// `-` when the platform can't supply the time.
fn human_time(t: Option<SystemTime>) -> String {
    match t {
        Some(t) => chrono::DateTime::<chrono::Utc>::from(t)
            .format("%Y-%m-%d %H:%M:%S%.9f +0000")
            .to_string(),
        None => "-".to_string(),
    }
}

/// Seconds since the epoch, or `0` (GNU stat prints `0` for unknown `%W`).
fn epoch_secs(t: Option<SystemTime>) -> String {
    match t.and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()) {
        Some(d) => d.as_secs().to_string(),
        None => "0".to_string(),
    }
}

/// The default (no `-c`) block, Linux-shaped with honest `-` for sandbox-unknowable fields.
fn render_default(info: &StatInfo) -> String {
    format!(
        "  File: {path}\n  \
         Size: {size}\tBlocks: -\tIO Block: -\t{kind}\n\
         Device: -\tInode: -\tLinks: -\n\
         Access: (-)\tUid: (-)\tGid: (-)\n\
         Access: {atime}\n\
         Modify: {mtime}\n\
         Change: -\n \
         Birth: {btime}\n",
        path = info.path,
        size = info.size,
        kind = info.kind,
        atime = human_time(info.accessed),
        mtime = human_time(info.modified),
        btime = human_time(info.created),
    )
}

/// Apply a `-c FORMAT` string: `%`-directives plus `\n`/`\t`/`\\` escapes; a trailing newline is
/// appended (GNU behavior). Unknown directives render as `?`.
fn render_format(info: &StatInfo, format: &str) -> String {
    let mut out = String::new();
    let mut chars = format.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '%' => match chars.next() {
                Some('n') => out.push_str(&info.path),
                Some('s') => out.push_str(&info.size.to_string()),
                Some('F') => out.push_str(info.kind),
                Some('y') => out.push_str(&human_time(info.modified)),
                Some('Y') => out.push_str(&epoch_secs(info.modified)),
                Some('x') => out.push_str(&human_time(info.accessed)),
                Some('X') => out.push_str(&epoch_secs(info.accessed)),
                Some('w') => out.push_str(&human_time(info.created)),
                Some('W') => out.push_str(&epoch_secs(info.created)),
                Some('%') => out.push('%'),
                Some(_) => out.push('?'),
                None => out.push('%'),
            },
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('\\') => out.push('\\'),
                Some(other) => out.push(other),
                None => out.push('\\'),
            },
            other => out.push(other),
        }
    }
    out.push('\n');
    out
}

impl SimpleCommand for Stat {
    fn get_content(
        name: &str,
        content_type: ContentType,
        _options: &ContentOptions,
    ) -> Result<String, Error> {
        match content_type {
            ContentType::ShortDescription => Ok(format!("{name} - {}\n", Stat::SYNOPSIS)),
            ContentType::ShortUsage => Ok(format!("{name}: {name} [-L] [-c FORMAT] FILE...\n")),
            ContentType::DetailedHelp => Ok(format!(
                "{name} - {}\n\nHonest wasm subset: size/type/timestamps are real; inode, uid/gid, \
                 mode bits, and blocks are not available in the sandbox and print as '-'.\n",
                Stat::SYNOPSIS
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
        let mut follow = false;
        let mut format: Option<String> = None;
        let mut operands: Vec<String> = Vec::new();
        let mut iter = argv.into_iter();
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "-L" | "--dereference" => follow = true,
                "-c" | "--format" => {
                    let Some(f) = iter.next() else {
                        let _ = writeln!(context.stderr(), "stat: option requires an argument: -c");
                        return Ok(ExecutionResult::new(2));
                    };
                    format = Some(f);
                }
                f if f.starts_with("--format=") => {
                    format = Some(f["--format=".len()..].to_string());
                }
                f if f.starts_with('-') && f.len() > 1 => {} // unknown flags: lenient, like ps
                op => operands.push(op.to_string()),
            }
        }

        if operands.is_empty() {
            let _ = writeln!(context.stderr(), "stat: missing operand");
            return Ok(ExecutionResult::new(2));
        }

        let mut out = context.stdout();
        let mut failed = false;
        for op in &operands {
            match resolve(op, follow) {
                Ok(info) => {
                    let rendered = match &format {
                        Some(f) => render_format(&info, f),
                        None => render_default(&info),
                    };
                    let _ = out.write_all(rendered.as_bytes());
                }
                Err(msg) => {
                    let _ = writeln!(context.stderr(), "stat: {msg}");
                    failed = true;
                }
            }
        }
        Ok(ExecutionResult::new(if failed { 1 } else { 0 }))
    }
}

pub(crate) fn builtins<SE: ShellExtensions>() -> Vec<(String, Registration<SE>)> {
    use brush_core::builtins::simple_builtin;
    vec![("stat".into(), simple_builtin::<Stat, SE>())]
}

pub(crate) fn manifests() -> Vec<Manifest> {
    vec![Manifest::builtin(Stat::NAME, Stat::SYNOPSIS).with_help(
        "stat [-L] [-c FORMAT] FILE... — display file status. Size, type, and timestamps are \
         real; inode, uid/gid, mode bits, and blocks are not available in the wasm sandbox and \
         print as '-'. FORMAT directives: %n name, %s size, %F type, %y/%Y mtime, %x/%X atime, \
         %w/%W birth, %% literal. Virtual paths (/bin, /proc) report as virtual read-only entries.",
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpfile(content: &[u8]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "clank-stat-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn resolve_real_file_reports_size_and_type() {
        let path = tmpfile(b"hello!");
        let info = resolve(path.to_str().unwrap(), false).unwrap();
        assert_eq!(info.size, 6);
        assert_eq!(info.kind, "regular file");
        assert!(info.modified.is_some());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn resolve_missing_file_is_an_error() {
        let err = resolve("/definitely/not/here", false).unwrap_err();
        assert!(err.contains("No such file or directory"), "got: {err}");
    }

    #[test]
    fn resolve_virtual_bin_entry() {
        let info = resolve("/bin/curl", false).unwrap();
        assert_eq!(info.kind, "virtual read-only file");
        assert!(info.size > 0);
        let dir = resolve("/bin", false).unwrap();
        assert_eq!(dir.kind, "directory");
    }

    #[test]
    fn default_format_has_honest_dashes() {
        let path = tmpfile(b"x");
        let info = resolve(path.to_str().unwrap(), false).unwrap();
        let rendered = render_default(&info);
        assert!(rendered.contains("Size: 1"));
        assert!(rendered.contains("regular file"));
        assert!(rendered.contains("Inode: -"));
        assert!(rendered.contains("Uid: (-)"));
        assert!(rendered.contains("Change: -"));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn format_directives_render() {
        let path = tmpfile(b"abc");
        let info = resolve(path.to_str().unwrap(), false).unwrap();
        assert_eq!(render_format(&info, "%s %F"), "3 regular file\n");
        assert!(render_format(&info, "%n").contains("clank-stat-test"));
        // Unknown directive → '?', escaped percent, backslash escapes.
        assert_eq!(render_format(&info, "%q%%\\t"), "?%\t\n");
        std::fs::remove_file(path).unwrap();
    }
}
