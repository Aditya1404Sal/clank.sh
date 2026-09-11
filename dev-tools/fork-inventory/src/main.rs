//! Regenerates `docs/FORKS.md` from the workspace root's `Cargo.toml` and `Cargo.lock`.
//!
//! `docs/FORKS.md` used to be hand-maintained prose. It said "two forks" when a third — a git
//! dependency on `golemcloud/wit-bindgen` — had been resolving into `Cargo.lock` transitively
//! (through the `golem-rust` path dependency) for months, named in no `Cargo.toml` in this repo.
//! Nobody lied; nobody re-checked `Cargo.lock` by hand after the fact either. That is the failure
//! mode this tool exists to close: it reads the two files that hold the ground truth and reports
//! exactly what they say, every time, so the report cannot go stale without `Cargo.lock` also
//! changing.
//!
//! Zero dependencies, on purpose (see the crate's `Cargo.toml`). Both input files are simple,
//! machine-formatted, line-oriented TOML — `key = "value"` and `key = { a = "..", b = ".." }`, one
//! entry per line, no multi-line inline tables in practice — so a hand-rolled line scanner is a
//! better trade here than a general TOML parser dependency would be.
//!
//! Run with `cargo run -p fork-inventory`. CI's `check-forks` step (`.github/workflows/
//! conformance.yml`, `hygiene` job) runs it and then `git diff --exit-code`s `docs/FORKS.md`, so a
//! pin change that lands without a regenerated table fails CI.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// One `[patch.crates-io]` entry from the root `Cargo.toml`.
///
/// Exactly one of `git`/`path` is set in every entry seen so far (a patch redirects crates.io
/// either to a git repo or to a local path, never both), but both are kept as `Option` rather than
/// an enum so a malformed future entry is reported by the renderer instead of rejected by the
/// parser — this tool's job is to describe the manifest, not to validate it.
struct PatchEntry {
    crate_name: String,
    git: Option<String>,
    path: Option<String>,
    /// `"rev"` or `"branch"` — which key pinned the git entry. `None` for path entries.
    pin_kind: Option<String>,
    /// The pinned rev or branch name as written in `Cargo.toml` (short form, e.g. `"35ecf24"`).
    pin_value: Option<String>,
}

/// One resolved `git+…` package source found in `Cargo.lock`.
struct LockGitPackage {
    name: String,
    version: String,
    /// The repository URL, with the `?rev=…`/`?branch=…` query and `#hash` fragment stripped.
    base_url: String,
    /// `"rev"`, `"branch"`, or `"HEAD"` if the lock source carried no query at all.
    pin_kind: String,
    pin_value: String,
    /// The full resolved commit hash — the part after `#` in the lock's `source` line.
    resolved_rev: String,
}

/// One `[submodule]` declared in `.gitmodules`, with the commit this repo pins it to.
///
/// A `[patch.crates-io]` entry can redirect a crate by `path` into a submodule's checkout. Cargo
/// then knows nothing about git — there is no `source` line in `Cargo.lock` — so the pin lives
/// entirely in git: the superproject's gitlink for the submodule path.
struct Submodule {
    /// The checkout path relative to the repo root, e.g. `fork/coreutils`.
    path: String,
    url: String,
    /// The `branch = …` key, if declared. Informational only: it steers `git submodule update
    /// --remote` and never an ordinary checkout, so it is NOT the pin.
    branch: Option<String>,
    /// The full commit hash the superproject's index records for `path`. This IS the pin — what a
    /// fresh `git submodule update --init` checks out.
    pinned: String,
}

fn main() -> ExitCode {
    match run() {
        Ok(summary) => {
            println!("{summary}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("fork-inventory: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<String, String> {
    let root = repo_root();
    let cargo_toml_path = root.join("Cargo.toml");
    let cargo_lock_path = root.join("Cargo.lock");
    let forks_md_path = root.join("docs").join("FORKS.md");

    let cargo_toml = read_file(&cargo_toml_path)?;
    let cargo_lock = read_file(&cargo_lock_path)?;

    let patches = parse_patch_crates_io(&cargo_toml)?;
    let lock_pkgs = parse_lock_git_packages(&cargo_lock);
    if lock_pkgs.is_empty() {
        return Err(
            "found no `git+` sources in Cargo.lock — the lockfile format likely changed and this \
             parser needs updating"
                .to_string(),
        );
    }
    let submodules = read_submodules(&root)?;

    let markdown = render_markdown(&patches, &lock_pkgs, &submodules);
    fs::write(&forks_md_path, &markdown)
        .map_err(|e| format!("failed to write {}: {e}", forks_md_path.display()))?;

    let git_count = patches.iter().filter(|p| p.git.is_some()).count();
    let (in_submodule, vendored) = partition_path_entries(&patches, &submodules);
    Ok(format!(
        "wrote {} — {} [patch.crates-io] entries ({git_count} git-pinned, {} in a submodule, {} \
         vendored path), {} git+ packages resolved in Cargo.lock",
        forks_md_path.display(),
        patches.len(),
        in_submodule.len(),
        vendored.len(),
        lock_pkgs.len(),
    ))
}

/// Every submodule declared in `.gitmodules`, each with the commit this repo pins it to. A missing
/// `.gitmodules` is not an error: a branch with no submodules simply has none to report.
fn read_submodules(root: &Path) -> Result<Vec<Submodule>, String> {
    let gitmodules_path = root.join(".gitmodules");
    let text = match fs::read_to_string(&gitmodules_path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("failed to read {}: {e}", gitmodules_path.display())),
    };
    parse_gitmodules(&text)?
        .into_iter()
        .map(|(path, url, branch)| {
            let pinned = gitlink_commit(root, &path)?;
            Ok(Submodule {
                path,
                url,
                branch,
                pinned,
            })
        })
        .collect()
}

/// Parses `.gitmodules` into `(path, url, branch)`, one per `[submodule "…"]` section.
///
/// A section missing `path` or `url` is an error rather than skipped: git itself rejects such a
/// section, and a silently dropped submodule is exactly the kind of hole this tool exists to close.
fn parse_gitmodules(text: &str) -> Result<Vec<(String, String, Option<String>)>, String> {
    #[derive(Default)]
    struct Section {
        path: Option<String>,
        url: Option<String>,
        branch: Option<String>,
    }

    let mut sections: Vec<Section> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("[submodule") {
            sections.push(Section::default());
            continue;
        }
        // A blank line, a `#`/`;` comment, or a key appearing before any section header.
        let (Some(section), Some((key, value))) = (sections.last_mut(), line.split_once('='))
        else {
            continue;
        };
        let value = Some(value.trim().to_string());
        match key.trim() {
            "path" => section.path = value,
            "url" => section.url = value,
            "branch" => section.branch = value,
            _ => {}
        }
    }
    sections
        .into_iter()
        .map(|s| match (s.path, s.url) {
            (Some(path), Some(url)) => Ok((path, url, s.branch)),
            _ => Err("a [submodule] section in .gitmodules is missing `path` or `url`".to_string()),
        })
        .collect()
}

/// The commit the superproject's index records for the submodule at `path` — its gitlink, and so
/// the actual pin. Read from the index rather than `HEAD` so a regenerated table matches what is
/// about to be committed; in CI the two are the same thing.
///
/// Shells out to `git`: the index is a binary format, and hand-parsing it to avoid a subprocess
/// would be the wrong trade for a CI helper that only ever runs where git already is.
fn gitlink_commit(root: &Path, path: &str) -> Result<String, String> {
    let out = std::process::Command::new("git")
        .args(["ls-files", "--stage", "--", path])
        .current_dir(root)
        .output()
        .map_err(|e| format!("failed to run `git ls-files` for submodule {path}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "`git ls-files --stage -- {path}` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let listing = String::from_utf8_lossy(&out.stdout);
    parse_gitlink(&listing).ok_or_else(|| {
        format!(
            "{path} is declared in .gitmodules but the index holds no gitlink for it \
             (`git ls-files --stage` printed {listing:?})"
        )
    })
}

/// The hash from a `git ls-files --stage` line, but only for a gitlink (mode `160000`). An ordinary
/// file or tree at that path means the "submodule" is not one.
fn parse_gitlink(listing: &str) -> Option<String> {
    let mut fields = listing.lines().next()?.split_whitespace();
    let (mode, hash) = (fields.next()?, fields.next()?);
    (mode == "160000").then(|| hash.to_string())
}

/// Splits the `path`-redirected `[patch.crates-io]` entries into those pointing inside a declared
/// submodule (pinned by its gitlink) and the rest (vendored: source committed in-tree).
fn partition_path_entries<'a>(
    patches: &'a [PatchEntry],
    submodules: &[Submodule],
) -> (Vec<&'a PatchEntry>, Vec<&'a PatchEntry>) {
    patches
        .iter()
        .filter(|p| p.path.is_some())
        .partition(|p| submodule_for(p, submodules).is_some())
}

/// The submodule whose checkout contains `entry`'s path, if any. Matches whole path components, so
/// `fork/coreutils-old/src` is NOT inside `fork/coreutils`.
fn submodule_for<'s>(entry: &PatchEntry, submodules: &'s [Submodule]) -> Option<&'s Submodule> {
    let path = entry.path.as_deref()?;
    submodules.iter().find(|s| {
        path.strip_prefix(s.path.as_str())
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    })
}

/// `dev-tools/fork-inventory` -> `dev-tools` -> the workspace root. `CARGO_MANIFEST_DIR` is a
/// compile-time constant naming this crate's own directory, so this is independent of the
/// caller's current working directory (unlike walking upward looking for a `Cargo.toml`, which
/// would also stop at the first one found and could pick the wrong ancestor in a worktree).
fn repo_root() -> PathBuf {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(Path::parent)
        .map_or_else(|| manifest_dir.to_path_buf(), Path::to_path_buf)
}

fn read_file(path: &Path) -> Result<String, String> {
    fs::read_to_string(path).map_err(|e| format!("failed to read {}: {e}", path.display()))
}

/// Parses every entry in the root `Cargo.toml`'s `[patch.crates-io]` table.
///
/// Errors loudly (rather than skipping) on a line it cannot parse or a missing/empty section —
/// this tool backs a CI gate, so a format drift it cannot handle must fail the build, not silently
/// emit a table with a hole in it.
fn parse_patch_crates_io(cargo_toml: &str) -> Result<Vec<PatchEntry>, String> {
    let mut in_section = false;
    let mut entries = Vec::new();

    for line in cargo_toml.lines() {
        let trimmed = line.trim();
        if trimmed == "[patch.crates-io]" {
            in_section = true;
            continue;
        }
        if !in_section {
            continue;
        }
        if trimmed.starts_with('[') {
            break; // the next table header — [patch.crates-io] has ended.
        }
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let entry = parse_patch_entry_line(trimmed)
            .ok_or_else(|| format!("could not parse [patch.crates-io] entry: {trimmed:?}"))?;
        entries.push(entry);
    }

    if !in_section {
        return Err("no [patch.crates-io] section found in Cargo.toml".to_string());
    }
    if entries.is_empty() {
        return Err("[patch.crates-io] section was found but produced no entries".to_string());
    }
    Ok(entries)
}

/// Parses one `crate = { git = "…", rev = "…" }` / `crate = { path = "…" }` line. Every entry in
/// this repo's `[patch.crates-io]` is a single-line inline table, so this does not handle a
/// multi-line `{ .. }` — [`parse_patch_crates_io`] surfaces that as an unparsed-line error rather
/// than silently mis-parsing it.
fn parse_patch_entry_line(line: &str) -> Option<PatchEntry> {
    let (key, rest) = line.split_once('=')?;
    let crate_name = key.trim().to_string();

    let open = rest.find('{')?;
    let close = rest.rfind('}')?;
    if close <= open {
        return None;
    }
    let inner = &rest[open + 1..close];

    let mut git = None;
    let mut path = None;
    let mut rev = None;
    let mut branch = None;
    for field in inner.split(',') {
        let field = field.trim();
        if field.is_empty() {
            continue;
        }
        let (k, v) = field.split_once('=')?;
        let value = v.trim().trim_matches('"').to_string();
        match k.trim() {
            "git" => git = Some(value),
            "path" => path = Some(value),
            "rev" => rev = Some(value),
            "branch" => branch = Some(value),
            _ => {} // e.g. a future `package = "…"` rename key — not needed for this report.
        }
    }

    let (pin_kind, pin_value) = match (rev, branch) {
        (Some(r), _) => (Some("rev".to_string()), Some(r)),
        (None, Some(b)) => (Some("branch".to_string()), Some(b)),
        (None, None) => (None, None),
    };

    Some(PatchEntry {
        crate_name,
        git,
        path,
        pin_kind,
        pin_value,
    })
}

/// Scans every `[[package]]` block in `Cargo.lock` and returns one [`LockGitPackage`] per package
/// whose `source` is a `git+…` URL — this is the ground-truth cross-check: it finds packages no
/// `Cargo.toml` in this repo names directly (arrived transitively) just as readily as ones that
/// match a `[patch.crates-io]` entry exactly.
fn parse_lock_git_packages(cargo_lock: &str) -> Vec<LockGitPackage> {
    let mut out = Vec::new();
    let mut cur_name = String::new();
    let mut cur_version = String::new();

    for raw_line in cargo_lock.lines() {
        let line = raw_line.trim();
        if line == "[[package]]" {
            cur_name.clear();
            cur_version.clear();
            continue;
        }
        if let Some(v) = extract_quoted(line, "name") {
            cur_name = v;
            continue;
        }
        if let Some(v) = extract_quoted(line, "version") {
            cur_version = v;
            continue;
        }
        if let Some(v) = extract_quoted(line, "source") {
            if let Some(pkg) = parse_git_source(&v, &cur_name, &cur_version) {
                out.push(pkg);
            }
        }
    }
    out
}

/// Extracts the value of a `key = "value"` line, tolerant of the spacing around `=`. Returns
/// `None` for any line that is not exactly that key (in particular, a quoted array element inside
/// a `dependencies = [...]` block never matches, since such lines start with `"`, not a bare key).
fn extract_quoted(line: &str, key: &str) -> Option<String> {
    let rest = line.strip_prefix(key)?;
    let rest = rest.trim_start().strip_prefix('=')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Parses a lockfile `source = "git+URL[?rev=…|branch=…]#hash"` value (the `git+` prefix already
/// stripped by the caller via [`extract_quoted`] — this takes the quoted value, so `source` here
/// still has `git+` on the front) into a [`LockGitPackage`], given the name/version already
/// collected for the current `[[package]]` block. Returns `None` for a non-git source (a plain
/// `registry+…` or `sparse+…` entry).
fn parse_git_source(source: &str, name: &str, version: &str) -> Option<LockGitPackage> {
    let rest = source.strip_prefix("git+")?;
    let (left, resolved_rev) = rest.rsplit_once('#')?;
    let (base_url, pin_kind, pin_value) = match left.split_once('?') {
        Some((url, query)) => {
            let (k, v) = query.split_once('=').unwrap_or(("branch", query));
            (url.to_string(), k.to_string(), v.to_string())
        }
        None => (left.to_string(), "HEAD".to_string(), String::new()),
    };
    Some(LockGitPackage {
        name: name.to_string(),
        version: version.to_string(),
        base_url,
        pin_kind,
        pin_value,
        resolved_rev: resolved_rev.to_string(),
    })
}

/// Renders the full `docs/FORKS.md` content. Pure function of its inputs — same `patches`,
/// `lock_pkgs` and `submodules`, same output, every time; that is what makes the `check-forks` CI
/// step meaningful. (The one input that is not a file, each submodule's gitlink, is read by [`run`]
/// and passed in here as plain data.) Split into one render function per section (each well under
/// clippy's line-count ceiling) rather than one long function — the sections don't share
/// intermediate state worth threading through as anything more than the slices/maps each needs.
fn render_markdown(
    patches: &[PatchEntry],
    lock_pkgs: &[LockGitPackage],
    submodules: &[Submodule],
) -> String {
    let git_entries: Vec<&PatchEntry> = patches.iter().filter(|p| p.git.is_some()).collect();
    let (submodule_entries, vendored_entries) = partition_path_entries(patches, submodules);
    let declared: HashSet<&str> = patches.iter().map(|p| p.crate_name.as_str()).collect();
    let resolved_by_name: HashMap<&str, &LockGitPackage> =
        lock_pkgs.iter().map(|p| (p.name.as_str(), p)).collect();

    let mut l: Vec<String> = Vec::new();
    l.extend(render_header());
    l.extend(render_summary(
        patches.len(),
        &git_entries,
        &submodule_entries,
        &vendored_entries,
        lock_pkgs,
        &declared,
    ));
    l.extend(render_table1(&git_entries, &resolved_by_name));
    l.extend(render_table2(lock_pkgs, &declared));
    l.extend(render_table3_submodules(&submodule_entries, submodules));
    l.extend(render_table4_vendored(&vendored_entries));
    l.extend(render_not_covered());
    format!("{}\n", l.join("\n"))
}

fn render_header() -> Vec<String> {
    vec![
        "# FORKS.md — generated fork inventory".to_string(),
        String::new(),
        "Generated by `dev-tools/fork-inventory` (`cargo run -p fork-inventory`) from this \
         repo's root `Cargo.toml` and `Cargo.lock`. **Do not hand-edit** — change the generator \
         (`dev-tools/fork-inventory/src/main.rs`) and regenerate. CI's `check-forks` step \
         (`.github/workflows/conformance.yml`, `hygiene` job) regenerates this file and runs \
         `git diff --exit-code` against it, so a pin change that lands without a regenerated \
         table fails CI."
            .to_string(),
        String::new(),
        "This file answers one narrow question: **what is patched, how is each pin anchored, and \
         does it survive a fresh `cargo update`.** For *why* a fork exists — the upstream bug or \
         missing wasip2 primitive that forced it — see [`docs/WASM_CHANGES.md`](WASM_CHANGES.md) \
         for the Brush and coreutils forks, and [§4](#4-vendored-path-forks) below for the \
         native-only vendored ones."
            .to_string(),
        String::new(),
    ]
}

/// `total_patches` is passed separately rather than derived as the sum of the three entry lists:
/// every entry seen in this repo so far has exactly one of `git`/`path` set, but that is a fact
/// about the data, not a fact this function should assume.
fn render_summary(
    total_patches: usize,
    git_entries: &[&PatchEntry],
    submodule_entries: &[&PatchEntry],
    vendored_entries: &[&PatchEntry],
    lock_pkgs: &[LockGitPackage],
    declared: &HashSet<&str>,
) -> Vec<String> {
    let branch_pinned_patch: Vec<&&PatchEntry> = git_entries
        .iter()
        .filter(|p| p.pin_kind.as_deref() == Some("branch"))
        .collect();
    let undeclared: Vec<&LockGitPackage> = lock_pkgs
        .iter()
        .filter(|p| !declared.contains(p.name.as_str()))
        .collect();
    let branch_pinned_lock: Vec<&LockGitPackage> = lock_pkgs
        .iter()
        .filter(|p| p.pin_kind == "branch")
        .collect();
    let distinct_repo_count = lock_pkgs
        .iter()
        .map(|p| p.base_url.as_str())
        .collect::<HashSet<_>>()
        .len();

    let mut l = vec!["## Summary".to_string(), String::new()];
    l.push(format!(
        "- `{total_patches}` `[patch.crates-io]` entries in root `Cargo.toml`: `{}` git-pinned, \
         `{}` redirected into a git submodule, `{}` vendored path (no git URL).",
        git_entries.len(),
        submodule_entries.len(),
        vendored_entries.len(),
    ));
    if !submodule_entries.is_empty() {
        l.push(
            "- Submodule-backed entries are pinned by this repo's **gitlink** for the submodule, \
             not by anything in `Cargo.toml` — reproducible from source control alone, *provided \
             the pinned commit has been pushed* to the submodule's remote. See \
             [§3](#3-submodule-path-forks)."
                .to_string(),
        );
    }
    if branch_pinned_patch.is_empty() {
        l.push(
            "- All git-pinned `[patch.crates-io]` entries are **rev**-pinned — reproducible from \
             source control alone."
                .to_string(),
        );
    } else {
        let names: Vec<&str> = branch_pinned_patch
            .iter()
            .map(|p| p.crate_name.as_str())
            .collect();
        l.push(format!(
            "- ⚠ **Branch-pinned (non-reproducible)** `[patch.crates-io]` entries — a fresh \
             resolve can silently move: {}.",
            backtick_join(&names),
        ));
    }
    l.push(format!(
        "- `Cargo.lock` resolves `{}` packages from `{distinct_repo_count}` distinct git \
         repositories.",
        lock_pkgs.len(),
    ));
    if undeclared.is_empty() {
        l.push(
            "- Every git+ package in `Cargo.lock` corresponds to a `[patch.crates-io]` entry — \
             nothing arrives transitively-only."
                .to_string(),
        );
    } else {
        let names: Vec<&str> = undeclared.iter().map(|p| p.name.as_str()).collect();
        l.push(format!(
            "- **`{}` of those packages are named in no `Cargo.toml` in this repo** — they \
             arrive transitively and are invisible to anyone reading the manifests alone: {}.",
            undeclared.len(),
            backtick_join(&names),
        ));
    }
    if !branch_pinned_lock.is_empty() {
        let names: Vec<&str> = branch_pinned_lock.iter().map(|p| p.name.as_str()).collect();
        l.push(format!(
            "- ⚠ **Branch-pinned in `Cargo.lock`** (not `rev`, so a fresh resolve can silently \
             move to a different commit): {}.",
            backtick_join(&names),
        ));
    }
    l.push(String::new());
    l
}

fn render_table1(
    git_entries: &[&PatchEntry],
    resolved_by_name: &HashMap<&str, &LockGitPackage>,
) -> Vec<String> {
    let mut l = vec![
        "## 1. `[patch.crates-io]` — git-pinned entries".to_string(),
        String::new(),
        "Every `[patch.crates-io]` entry that redirects crates.io to a git repository, with the \
         rev actually resolved into `Cargo.lock` alongside the short form written in \
         `Cargo.toml`. Entries redirected by `path` are in [§3](#3-submodule-path-forks) (into a \
         git submodule) and [§4](#4-vendored-path-forks) (vendored in-tree)."
            .to_string(),
        String::new(),
        "| Crate | Fork repository | Pin kind | Pinned value | Resolved rev (Cargo.lock) |"
            .to_string(),
        "|---|---|---|---|---|".to_string(),
    ];
    for entry in git_entries {
        let git = entry.git.as_deref().unwrap_or("?");
        let kind = entry.pin_kind.as_deref().unwrap_or("?");
        let value = entry.pin_value.as_deref().unwrap_or("?");
        let resolved = resolved_by_name
            .get(entry.crate_name.as_str())
            .map_or("(not found in Cargo.lock — is this patch unused?)", |p| {
                p.resolved_rev.as_str()
            });
        l.push(format!(
            "| `{}` | {git} | {kind} | `{value}` | `{resolved}` |",
            entry.crate_name,
        ));
    }
    l.push(String::new());
    l
}

fn render_table2(lock_pkgs: &[LockGitPackage], declared: &HashSet<&str>) -> Vec<String> {
    let mut l = vec![
        "## 2. `git+` sources resolved in `Cargo.lock`".to_string(),
        String::new(),
        "Every package Cargo actually resolved from a git repository — the ground truth, \
         cross-checked against Table 1. **`Declared?` = No** means the package is pulled in \
         transitively by another patched crate's own dependency graph and is named in no \
         `Cargo.toml` in this repo; it would not appear in a fork inventory written by reading \
         the manifests."
            .to_string(),
        String::new(),
        "| Package | Version | Repository | Pin kind | Pinned value | Resolved rev | Declared \
         in `[patch.crates-io]`? |"
            .to_string(),
        "|---|---|---|---|---|---|---|".to_string(),
    ];
    for pkg in lock_pkgs {
        let declared_str = if declared.contains(pkg.name.as_str()) {
            "Yes"
        } else {
            "**No**"
        };
        l.push(format!(
            "| `{}` | {} | {} | {} | `{}` | `{}` | {declared_str} |",
            pkg.name, pkg.version, pkg.base_url, pkg.pin_kind, pkg.pin_value, pkg.resolved_rev,
        ));
    }
    l.push(String::new());
    l
}

fn render_table3_submodules(entries: &[&PatchEntry], submodules: &[Submodule]) -> Vec<String> {
    let mut l = vec![
        "## 3. Submodule path-forks".to_string(),
        String::new(),
        "Redirected by `path` into a directory that is a **git submodule** of this repo. Cargo \
         sees only a local path, so there is no `source` line in `Cargo.lock`; the pin is this \
         repo's **gitlink** for the submodule — the commit recorded in its own tree, read here \
         from the index. A fresh `git submodule update --init` checks out exactly that commit, \
         which makes it as reproducible as a `rev` pin, but only once the commit has been pushed \
         to the submodule's remote: until then every local build passes while CI and fresh clones \
         cannot fetch it. The submodule's own workspace may also resolve further crates from the \
         same checkout transitively (a proc-macro the patched crates depend on, say); those are \
         path packages too and ride the same pin."
            .to_string(),
        String::new(),
    ];
    if entries.is_empty() {
        l.push("No `[patch.crates-io]` entry points into a git submodule.".to_string());
        l.push(String::new());
        return l;
    }
    for sub in submodules {
        let crates: Vec<&&PatchEntry> = entries
            .iter()
            .filter(|e| submodule_for(e, submodules).is_some_and(|s| s.path == sub.path))
            .collect();
        if crates.is_empty() {
            continue;
        }
        l.push(format!("### `{}`", sub.path));
        l.push(String::new());
        l.push(format!("- **Repository:** {}", sub.url));
        if let Some(branch) = &sub.branch {
            l.push(format!(
                "- **Tracking branch:** `{branch}` — steers `git submodule update --remote` only; \
                 it is not the pin."
            ));
        }
        l.push(format!("- **Pinned commit (gitlink):** `{}`", sub.pinned));
        l.push(format!("- **Redirects `{}` crates:**", crates.len()));
        l.push(String::new());
        l.push("| Crate | Path |".to_string());
        l.push("|---|---|".to_string());
        for entry in crates {
            let path = entry.path.as_deref().unwrap_or("?");
            l.push(format!("| `{}` | `{path}` |", entry.crate_name));
        }
        l.push(String::new());
    }
    l
}

fn render_table4_vendored(path_entries: &[&PatchEntry]) -> Vec<String> {
    let mut l = vec![
        "## 4. Vendored path-forks".to_string(),
        String::new(),
        "Not git dependencies — the fork's source is committed in-tree, and \
         `[patch.crates-io]` points crates.io lookups at a local path instead of a registry or \
         git source. There is no URL or rev to resolve, so these do not appear in `Cargo.lock` \
         with a `source` line at all (`cargo` treats a local path dependency as needing no \
         external source). Both are native-only and never enter the wasm32-wasip2 build."
            .to_string(),
        String::new(),
        "| Crate | Vendored at | Why (native-only) |".to_string(),
        "|---|---|---|".to_string(),
    ];
    for entry in path_entries {
        let path = entry.path.as_deref().unwrap_or("?");
        let why = vendored_fork_rationale(&entry.crate_name);
        l.push(format!("| `{}` | `{path}/` | {why} |", entry.crate_name));
    }
    l.push(String::new());
    l
}

fn render_not_covered() -> Vec<String> {
    vec![
        "## Not covered by this table".to_string(),
        String::new(),
        "- **The `golem-stuff/golem` clone's `agent shell` patch** is not a Cargo dependency — \
         nothing in `Cargo.toml` or `Cargo.lock` points at it, so a generator driven by those \
         two files structurally cannot see it. It builds a separate CLI *binary* (`golem`), not \
         a crate clank links; see `DEV_SDK_CHANGES.md` for how that clone also supplies the dev \
         Golem SDK path dependency on this branch. A prior hand-written revision of this file \
         carried the fork's full history (the PR #3700 vouch-bot rejection, the rebase \
         procedure) — see git history at `c6851a5` if that writeup needs a home again; it \
         wasn't ported here because it describes something outside what this tool reads."
            .to_string(),
        String::new(),
    ]
}

/// The one-line "why" for a vendored native-only fork, kept as source text here (not derived from
/// the manifests — there is nothing in `Cargo.toml` that explains intent) rather than in the
/// generated table body, so it stays reproducible across runs without going stale on its own: it
/// changes only when this generator's source does.
fn vendored_fork_rationale(crate_name: &str) -> &'static str {
    match crate_name {
        "reedline" => {
            "native REPL line editor; one-line patch makes `initialize_prompt_position`'s \
             `cursor::position()` timeout tolerant (falls back to col 0, bottom row) instead of \
             aborting `read_line` when a terminal answers the DSR query late — which is exactly \
             what happens right after `ask` dumps a burst of output."
        }
        "crossterm" => {
            "terminal backend paired with the `reedline` fork above; cuts two DSR-reply timeouts \
             from 2000ms to 250ms so a slow-to-answer terminal fails the query fast instead of \
             blocking ~1s at every prompt. Only crossterm 0.28 (reedline's) is patched — brush's \
             crossterm 0.25 is a distinct resolved version, untouched."
        }
        _ => "(no rationale recorded for this crate — see the fork's own commit history)",
    }
}

/// Joins names as `` `a`, `b`, `c` `` for the Summary bullets.
fn backtick_join(names: &[&str]) -> String {
    names
        .iter()
        .map(|n| format!("`{n}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_rev_pinned_git_patch_entry() {
        let entry = parse_patch_entry_line(
            r#"uucore = { git = "https://github.com/Aditya1404Sal/coreutils", rev = "35ecf24" }"#,
        )
        .expect("should parse");
        assert_eq!(entry.crate_name, "uucore");
        assert_eq!(
            entry.git.as_deref(),
            Some("https://github.com/Aditya1404Sal/coreutils")
        );
        assert_eq!(entry.pin_kind.as_deref(), Some("rev"));
        assert_eq!(entry.pin_value.as_deref(), Some("35ecf24"));
        assert_eq!(entry.path, None);
    }

    #[test]
    fn parses_a_branch_pinned_git_patch_entry() {
        let entry = parse_patch_entry_line(
            r#"wit-bindgen = { git = "https://github.com/golemcloud/wit-bindgen", branch = "main" }"#,
        )
        .expect("should parse");
        assert_eq!(entry.pin_kind.as_deref(), Some("branch"));
        assert_eq!(entry.pin_value.as_deref(), Some("main"));
    }

    #[test]
    fn parses_a_vendored_path_patch_entry() {
        let entry = parse_patch_entry_line(r#"reedline = { path = "fork/reedline" }"#)
            .expect("should parse");
        assert_eq!(entry.crate_name, "reedline");
        assert_eq!(entry.path.as_deref(), Some("fork/reedline"));
        assert_eq!(entry.git, None);
        assert_eq!(entry.pin_kind, None);
    }

    #[test]
    fn extract_quoted_reads_name_version_and_source() {
        assert_eq!(
            extract_quoted(r#"name = "wit-bindgen""#, "name").as_deref(),
            Some("wit-bindgen")
        );
        assert_eq!(
            extract_quoted(r#"version = "0.59.0""#, "version").as_deref(),
            Some("0.59.0")
        );
        // A quoted dependency-array element must never match a bare `name = "..."` line.
        assert_eq!(extract_quoted(r#""serde 1.0.219","#, "name"), None);
    }

    #[test]
    fn parse_git_source_splits_rev_kind_and_hash() {
        let pkg = parse_git_source(
            "git+https://github.com/Aditya1404Sal/brush?rev=02de798#02de798167b633fca57dd81efe253afa18f124d4",
            "brush-core",
            "0.5.0",
        )
        .expect("should parse");
        assert_eq!(pkg.base_url, "https://github.com/Aditya1404Sal/brush");
        assert_eq!(pkg.pin_kind, "rev");
        assert_eq!(pkg.pin_value, "02de798");
        assert_eq!(pkg.resolved_rev, "02de798167b633fca57dd81efe253afa18f124d4");
    }

    #[test]
    fn parse_git_source_splits_branch_kind() {
        let pkg = parse_git_source(
            "git+https://github.com/golemcloud/wit-bindgen?branch=golem-outline-lift-v0.58.0#4407232ead86d9bcbd06cbebd790a52120a4087a",
            "wit-bindgen",
            "0.59.0",
        )
        .expect("should parse");
        assert_eq!(pkg.pin_kind, "branch");
        assert_eq!(pkg.pin_value, "golem-outline-lift-v0.58.0");
    }

    #[test]
    fn parse_git_source_rejects_non_git_sources() {
        assert!(parse_git_source(
            "registry+https://github.com/rust-lang/crates.io-index",
            "serde",
            "1.0.219"
        )
        .is_none());
    }

    #[test]
    fn render_flags_undeclared_and_branch_pinned_packages() {
        let patches = vec![PatchEntry {
            crate_name: "uucore".to_string(),
            git: Some("https://github.com/Aditya1404Sal/coreutils".to_string()),
            path: None,
            pin_kind: Some("rev".to_string()),
            pin_value: Some("35ecf24".to_string()),
        }];
        let lock_pkgs = vec![
            LockGitPackage {
                name: "uucore".to_string(),
                version: "0.9.0".to_string(),
                base_url: "https://github.com/Aditya1404Sal/coreutils".to_string(),
                pin_kind: "rev".to_string(),
                pin_value: "35ecf24".to_string(),
                resolved_rev: "35ecf24d7caa2202940a18ef61be5037776ecd36".to_string(),
            },
            LockGitPackage {
                name: "wit-bindgen".to_string(),
                version: "0.59.0".to_string(),
                base_url: "https://github.com/golemcloud/wit-bindgen".to_string(),
                pin_kind: "branch".to_string(),
                pin_value: "golem-outline-lift-v0.58.0".to_string(),
                resolved_rev: "4407232ead86d9bcbd06cbebd790a52120a4087a".to_string(),
            },
        ];
        let md = render_markdown(&patches, &lock_pkgs, &[]);
        assert!(md.contains("`wit-bindgen`"));
        assert!(md.contains("**No**"));
        assert!(md.contains("Branch-pinned in `Cargo.lock`"));
    }

    #[test]
    fn parse_gitmodules_reads_path_url_and_branch() {
        let parsed = parse_gitmodules(
            "[submodule \"fork/coreutils\"]\n\
             \tpath = fork/coreutils\n\
             \turl = https://github.com/Aditya1404Sal/coreutils\n\
             \tbranch = wasip2-oscompat\n\
             [submodule \"other\"]\n\
             \tpath = other\n\
             \turl = https://example.com/other\n",
        )
        .expect("should parse");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].0, "fork/coreutils");
        assert_eq!(parsed[0].1, "https://github.com/Aditya1404Sal/coreutils");
        assert_eq!(parsed[0].2.as_deref(), Some("wasip2-oscompat"));
        assert_eq!(parsed[1].2, None);
    }

    #[test]
    fn parse_gitmodules_rejects_a_section_missing_its_url() {
        assert!(parse_gitmodules("[submodule \"x\"]\n\tpath = x\n").is_err());
    }

    #[test]
    fn parse_gitlink_accepts_only_a_gitlink() {
        let hash = "35ecf24d7caa2202940a18ef61be5037776ecd36";
        assert_eq!(
            parse_gitlink(&format!("160000 {hash} 0\tfork/coreutils\n")).as_deref(),
            Some(hash)
        );
        // An ordinary file at the path means it is not a submodule at all.
        assert_eq!(
            parse_gitlink(&format!("100644 {hash} 0\tfork/coreutils\n")),
            None
        );
        assert_eq!(parse_gitlink(""), None);
    }

    fn coreutils_submodule() -> Submodule {
        Submodule {
            path: "fork/coreutils".to_string(),
            url: "https://github.com/Aditya1404Sal/coreutils".to_string(),
            branch: Some("wasip2-oscompat".to_string()),
            pinned: "35ecf24d7caa2202940a18ef61be5037776ecd36".to_string(),
        }
    }

    fn path_patch(crate_name: &str, path: &str) -> PatchEntry {
        PatchEntry {
            crate_name: crate_name.to_string(),
            git: None,
            path: Some(path.to_string()),
            pin_kind: None,
            pin_value: None,
        }
    }

    #[test]
    fn submodule_for_matches_whole_path_components_only() {
        let subs = [coreutils_submodule()];
        assert!(submodule_for(&path_patch("uucore", "fork/coreutils/src/uucore"), &subs).is_some());
        assert!(submodule_for(&path_patch("x", "fork/coreutils"), &subs).is_some());
        // A sibling that merely shares the prefix is NOT inside the submodule.
        assert!(submodule_for(&path_patch("x", "fork/coreutils-old/src"), &subs).is_none());
        assert!(submodule_for(&path_patch("reedline", "fork/reedline"), &subs).is_none());
    }

    #[test]
    fn render_puts_submodule_entries_in_section_3_with_their_gitlink() {
        let patches = vec![
            path_patch("uucore", "fork/coreutils/src/uucore"),
            path_patch("reedline", "fork/reedline"),
        ];
        let md = render_markdown(&patches, &[], &[coreutils_submodule()]);
        assert!(md.contains("`1` redirected into a git submodule, `1` vendored path"));
        assert!(
            md.contains("**Pinned commit (gitlink):** `35ecf24d7caa2202940a18ef61be5037776ecd36`")
        );
        assert!(md.contains("| `uucore` | `fork/coreutils/src/uucore` |"));
        // The vendored fork lands in §4, not in the submodule section.
        let section_4 = md
            .split("## 4. Vendored path-forks")
            .nth(1)
            .expect("§4 is rendered");
        assert!(section_4.contains("| `reedline` | `fork/reedline/` |"));
    }

    #[test]
    fn render_is_deterministic_across_repeated_calls() {
        let patches = vec![PatchEntry {
            crate_name: "reedline".to_string(),
            git: None,
            path: Some("fork/reedline".to_string()),
            pin_kind: None,
            pin_value: None,
        }];
        let lock_pkgs = vec![LockGitPackage {
            name: "uucore".to_string(),
            version: "0.9.0".to_string(),
            base_url: "https://github.com/Aditya1404Sal/coreutils".to_string(),
            pin_kind: "rev".to_string(),
            pin_value: "35ecf24".to_string(),
            resolved_rev: "35ecf24d7caa2202940a18ef61be5037776ecd36".to_string(),
        }];
        let subs = [coreutils_submodule()];
        let first = render_markdown(&patches, &lock_pkgs, &subs);
        let second = render_markdown(&patches, &lock_pkgs, &subs);
        assert_eq!(first, second);
    }
}
