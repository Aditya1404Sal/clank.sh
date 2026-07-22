//! `grease-populate` — an interactive authoring tool + live HTTP server for a grease registry.
//!
//! One process: it serves a registry directory over HTTP (so `grease registry add` can point at it)
//! AND walks you through authoring every package kind — prompts (inline or from a `.md`), scripts
//! (from a `.sh`), skills (`.md` docs + `.sh` scripts), agents, and mcp. Each package is signed +
//! content-hashed + given an RFC-6962 single-leaf transparency proof using the SAME
//! `clank_shell::grease::pkg` code the durable agent verifies with — so what this tool emits is,
//! by construction, exactly what `grease install` accepts.
//!
//! Usage:  grease-populate <registry-dir> [--port <n>] [--signer <name>]
//!
//! The authoring core (`Registry`, `write_package`, `serve`) is inquire-free and unit-tested against
//! the real verifier; the interactive layer is a thin adapter over it.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use ed25519_dalek::{Signer, SigningKey};
use serde::Serialize;
use serde_json::{json, Value};

use clank_shell::grease::pkg::{
    sha256_hex, AgentMethod, AgentPackage, PackageArg, PromptPackage, ScriptPackage, SkillDocument,
    SkillPackage, SkillScript,
};

// ---------------------------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------------------------

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn b64_decode(s: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(s.trim())
        .context("invalid base64")
}

/// The RFC-6962 leaf hash `sha256(0x00 ‖ data)` — identical to the lib's private `rfc6962_leaf_hash`
/// (kept in lockstep; the single-leaf tree is all this tool builds).
fn leaf_hash(data: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update([0x00]);
    h.update(data);
    h.finalize().into()
}

/// `""` → `None`, else `Some(trimmed-original)` (blank means "not set" for optional fields).
fn opt(s: String) -> Option<String> {
    if s.trim().is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Serialize a typed package to JSON bytes with the `kind` discriminant injected (the payload structs
/// carry no `kind` field; `grease` reads it separately). The exact bytes returned are what we write,
/// hash, sign, and serve — one source of truth.
fn payload_with_kind<T: Serialize>(pkg: &T, kind: &str) -> Result<Vec<u8>> {
    let mut v = serde_json::to_value(pkg).context("serialize package")?;
    if let Value::Object(map) = &mut v {
        map.insert("kind".to_string(), Value::String(kind.to_string()));
    }
    serde_json::to_vec_pretty(&v).context("encode payload")
}

// ---------------------------------------------------------------------------------------------
// Registry — the pure authoring core (no inquire)
// ---------------------------------------------------------------------------------------------

struct Registry {
    dir: PathBuf,
    key: SigningKey,
    signer: String,
    /// The in-memory index entries (upserted by name), mirrored to `index.json` on every write.
    entries: Vec<Value>,
}

impl Registry {
    /// Open (or create) a registry at `dir`: load an existing `index.json` (append, don't clobber),
    /// and load-or-generate the signing key persisted at `<dir>/.signing-seed`.
    fn open(dir: &Path, signer: &str) -> Result<Self> {
        std::fs::create_dir_all(dir.join("packages")).context("create packages dir")?;
        let key = load_or_create_key(dir)?;
        let entries = match std::fs::read(dir.join("index.json")) {
            Ok(bytes) => serde_json::from_slice::<Value>(&bytes)
                .ok()
                .and_then(|v| v.get("packages").and_then(|p| p.as_array()).cloned())
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        Ok(Self { dir: dir.to_path_buf(), key, signer: signer.to_string(), entries })
    }

    /// The base64 public key to hand to `grease registry add --key`.
    fn pubkey_b64(&self) -> String {
        b64(&self.key.verifying_key().to_bytes())
    }

    /// Write one package: the payload file, plus a signed + content-hashed + single-leaf-logged index
    /// entry (upserted by name). Rewrites `index.json`. This is the whole security surface, and it is
    /// what the unit test exercises against the real verifier.
    fn write_package(
        &mut self,
        kind: &str,
        name: &str,
        description: &str,
        payload: &[u8],
        ext: &str,
    ) -> Result<()> {
        if name.is_empty() || name.contains('/') || name.contains("..") {
            bail!("invalid package name {name:?}");
        }
        std::fs::write(self.dir.join("packages").join(format!("{name}.{ext}")), payload)
            .context("write payload")?;

        let sha = sha256_hex(payload);
        let sig = b64(&self.key.sign(payload).to_bytes());
        // Single-leaf RFC-6962 log: the leaf is the sha256-HEX string bytes; root = leaf_hash(leaf);
        // tree-size 1, empty proof — exactly what `verify_inclusion_proof(leaf, 0, 1, root, &[])` checks.
        let root = b64(&leaf_hash(sha.as_bytes()));
        let entry = json!({
            "name": name,
            "kind": kind,
            "description": description,
            "sha256": sha,
            "sig": sig,
            "signer": self.signer,
            "log": { "leaf-index": 0, "tree-size": 1, "root": root, "proof": [] },
        });

        self.entries.retain(|e| e.get("name").and_then(Value::as_str) != Some(name));
        self.entries.push(entry);
        self.save_index()
    }

    fn save_index(&self) -> Result<()> {
        let index = json!({ "packages": self.entries });
        std::fs::write(
            self.dir.join("index.json"),
            serde_json::to_string_pretty(&index).context("encode index")?,
        )
        .context("write index.json")
    }
}

/// Load the persisted signing key, or generate one from `/dev/urandom` and persist its 32-byte seed
/// (base64) at `<dir>/.signing-seed` so the public key is stable across runs.
fn load_or_create_key(dir: &Path) -> Result<SigningKey> {
    let seed_path = dir.join(".signing-seed");
    if let Ok(text) = std::fs::read_to_string(&seed_path) {
        let bytes = b64_decode(&text)?;
        let seed: [u8; 32] = bytes.try_into().map_err(|_| anyhow!("stored seed is not 32 bytes"))?;
        return Ok(SigningKey::from_bytes(&seed));
    }
    let mut seed = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .context("open /dev/urandom")?
        .read_exact(&mut seed)
        .context("read entropy")?;
    std::fs::write(&seed_path, b64(&seed)).context("persist signing seed")?;
    Ok(SigningKey::from_bytes(&seed))
}

// ---------------------------------------------------------------------------------------------
// The live HTTP server (std::net only) — serves the registry directory
// ---------------------------------------------------------------------------------------------

/// Spawn a minimal static HTTP/1.1 server over `dir` on `127.0.0.1:port`, thread-per-connection.
/// Serves `GET /index.json` and `GET /packages/<name>` — the only paths `grease` fetches. Returns the
/// actually-bound port (so a caller can pass `0` for an ephemeral one).
fn serve(dir: PathBuf, port: u16) -> Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .with_context(|| format!("bind 127.0.0.1:{port} (already in use?)"))?;
    let bound = listener.local_addr().context("local_addr")?.port();
    let dir = Arc::new(dir);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let dir = Arc::clone(&dir);
            std::thread::spawn(move || {
                let _ = handle_conn(stream, &dir);
            });
        }
    });
    Ok(bound)
}

fn handle_conn(mut stream: TcpStream, dir: &Path) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    // "GET /path HTTP/1.1"
    let path = request_line.split_whitespace().nth(1).unwrap_or("/");
    let rel = path.split('?').next().unwrap_or("/").trim_start_matches('/');

    // Path-traversal guard: only serve plain names under the registry dir.
    let body = if rel.is_empty() || rel.contains("..") || rel.starts_with('/') {
        None
    } else {
        std::fs::read(dir.join(rel)).ok()
    };

    match body {
        Some(bytes) => {
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n",
                bytes.len()
            );
            stream.write_all(header.as_bytes())?;
            stream.write_all(&bytes)?;
        }
        None => {
            let msg = b"not found\n";
            let header = format!(
                "HTTP/1.1 404 Not Found\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                msg.len()
            );
            stream.write_all(header.as_bytes())?;
            stream.write_all(msg)?;
        }
    }
    stream.flush()
}

// ---------------------------------------------------------------------------------------------
// Interactive UI (inquire) — a thin adapter over Registry
// ---------------------------------------------------------------------------------------------

use inquire::{Confirm, Editor, Select, Text};

fn text(msg: &str) -> Result<String> {
    Ok(Text::new(msg).prompt()?)
}

fn confirm(msg: &str) -> Result<bool> {
    Ok(Confirm::new(msg).with_default(false).prompt()?)
}

/// Collect a package's `arguments` list (`{{name}}` substitution params), one at a time.
fn collect_args() -> Result<Vec<PackageArg>> {
    let mut args = Vec::new();
    while confirm("add an argument?")? {
        let name = text("  arg name")?;
        let description = text("  arg description")?;
        let required = confirm("  required?")?;
        let default = opt(text("  default value (blank = none)")?);
        args.push(PackageArg { name, description, required, default });
    }
    Ok(args)
}

/// A body from a file (`.sh`/`.md`) or typed inline via $EDITOR.
fn body_from_file_or_editor(kind_label: &str) -> Result<String> {
    let src = Select::new(
        &format!("{kind_label} body source"),
        vec!["from a file", "type inline (opens $EDITOR)"],
    )
    .prompt()?;
    if src == "from a file" {
        let path = text("  file path")?;
        std::fs::read_to_string(&path).with_context(|| format!("read {path}"))
    } else {
        Ok(Editor::new(&format!("{kind_label} body")).prompt()?)
    }
}

fn author_prompt(reg: &mut Registry) -> Result<()> {
    let src = Select::new("prompt source", vec!["inline", "from a .md file (YAML frontmatter)"])
        .prompt()?;
    if src == "inline" {
        let name = text("name")?;
        let description = text("description")?;
        let model = opt(text("model (blank = default)")?);
        let arguments = collect_args()?;
        let body = Editor::new("prompt body").prompt()?;
        let pkg = PromptPackage { name: name.clone(), description: description.clone(), model, arguments, body };
        let payload = payload_with_kind(&pkg, "prompt")?;
        reg.write_package("prompt", &name, &description, &payload, "json")?;
        announce(reg, &name);
    } else {
        let path = text(".md file path")?;
        let bytes = std::fs::read(&path).with_context(|| format!("read {path}"))?;
        if !(bytes.starts_with(b"---\n") || bytes.starts_with(b"---\r\n")) {
            bail!("{path} does not start with a `---` YAML frontmatter fence");
        }
        let name = text("package name (the registry filename)")?;
        let description = text("description (for the index listing)")?;
        // The payload is the RAW .md bytes — integrity is verified over exactly these.
        reg.write_package("prompt", &name, &description, &bytes, "md")?;
        announce(reg, &name);
    }
    Ok(())
}

fn author_script(reg: &mut Registry) -> Result<()> {
    let name = text("name")?;
    let description = text("description")?;
    let body = body_from_file_or_editor("script")?;
    let arguments = collect_args()?;
    let pkg = ScriptPackage { name: name.clone(), description: description.clone(), arguments, body };
    let payload = payload_with_kind(&pkg, "script")?;
    reg.write_package("script", &name, &description, &payload, "json")?;
    announce(reg, &name);
    Ok(())
}

fn author_skill(reg: &mut Registry) -> Result<()> {
    let name = text("name")?;
    let description = text("description")?;
    let intended_use = opt(text("intended use (when the model should consult this skill)")?);

    let mut documents = Vec::new();
    while confirm("add a document?")? {
        let path = text("  document path within the skill (e.g. SKILL.md)")?;
        let content = body_from_file_or_editor("document")?;
        documents.push(SkillDocument { path, content });
    }
    let mut scripts = Vec::new();
    while confirm("add a bundled script?")? {
        let sname = text("  script name")?;
        let body = body_from_file_or_editor("script")?;
        scripts.push(SkillScript { name: sname, body });
    }

    let pkg = SkillPackage {
        name: name.clone(),
        description: description.clone(),
        intended_use,
        documents,
        scripts,
    };
    let payload = payload_with_kind(&pkg, "skill")?;
    reg.write_package("skill", &name, &description, &payload, "json")?;
    announce(reg, &name);
    Ok(())
}

fn author_agent(reg: &mut Registry) -> Result<()> {
    let name = text("name")?;
    let description = text("description")?;
    let agent_type = text("agent type (the deployed Golem agent type, e.g. GreeterAgent)")?;
    let constructor_params = split_csv(&text("constructor params (comma-separated names)")?);

    let mut methods = Vec::new();
    while confirm("add a method?")? {
        let mname = text("  method name")?;
        let mdesc = text("  method description")?;
        let params = split_csv(&text("  method params (comma-separated names)")?);
        methods.push(AgentMethod { name: mname, description: mdesc, params });
    }
    let ephemeral = confirm("ephemeral (stateless) agent?")?;

    let pkg = AgentPackage {
        name: name.clone(),
        description: description.clone(),
        agent_type,
        constructor_params,
        methods,
        ephemeral,
    };
    let payload = payload_with_kind(&pkg, "agent")?;
    reg.write_package("agent", &name, &description, &payload, "json")?;
    announce(reg, &name);
    Ok(())
}

fn author_mcp(reg: &mut Registry) -> Result<()> {
    let name = text("name")?;
    let description = text("description")?;
    let url = text("MCP server URL (https://…)")?;
    let auth_env = opt(text("auth env var (blank = none)")?);
    // Emit the minimal MCP payload directly — the cache fields (tools/prompts/resources) are
    // `#[serde(default)]` and the agent enriches them at install time.
    let mut map = serde_json::Map::new();
    map.insert("kind".into(), json!("mcp"));
    map.insert("name".into(), json!(name));
    map.insert("description".into(), json!(description));
    map.insert("url".into(), json!(url));
    if let Some(env) = auth_env {
        map.insert("auth-env".into(), json!(env));
    }
    let payload = serde_json::to_vec_pretty(&Value::Object(map)).context("encode mcp payload")?;
    reg.write_package("mcp", &name, &description, &payload, "json")?;
    announce(reg, &name);
    Ok(())
}

fn split_csv(s: &str) -> Vec<String> {
    s.split(',').map(|w| w.trim().to_string()).filter(|w| !w.is_empty()).collect()
}

fn list(reg: &Registry) {
    if reg.entries.is_empty() {
        println!("  (registry is empty)");
        return;
    }
    for e in &reg.entries {
        println!(
            "  {:<20} [{}]  {}",
            e.get("name").and_then(Value::as_str).unwrap_or("?"),
            e.get("kind").and_then(Value::as_str).unwrap_or("?"),
            e.get("description").and_then(Value::as_str).unwrap_or(""),
        );
    }
}

fn announce(reg: &Registry, name: &str) {
    println!("  ✔ signed + logged '{name}' → run:  grease install {name}");
    let _ = reg;
}

// ---------------------------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------------------------

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let dir = args
        .get(1)
        .filter(|a| !a.starts_with('-'))
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("usage: grease-populate <registry-dir> [--port <n>] [--signer <name>]"))?;
    let port = flag_value(&args, "--port").map(|s| s.parse()).transpose()?.unwrap_or(8823u16);
    let signer = flag_value(&args, "--signer").unwrap_or_else(|| "grease-populate".to_string());

    let mut reg = Registry::open(&dir, &signer)?;
    let port = serve(dir.clone(), port)?;

    let pubkey = reg.pubkey_b64();
    println!("grease-populate — registry: {}", dir.display());
    println!("serving:  http://localhost:{port}");
    println!("pubkey:   {pubkey}");
    println!("\nIn a clank shell, trust + install from this registry with:");
    println!("  grease registry add http://localhost:{port} --key {pubkey}\n");

    loop {
        let choice = match Select::new(
            "add to the registry:",
            vec!["prompt", "script", "skill", "agent", "mcp", "list", "quit"],
        )
        .prompt()
        {
            Ok(c) => c,
            // Esc / Ctrl-C at the menu = quit cleanly.
            Err(_) => break,
        };
        let result = match choice {
            "prompt" => author_prompt(&mut reg),
            "script" => author_script(&mut reg),
            "skill" => author_skill(&mut reg),
            "agent" => author_agent(&mut reg),
            "mcp" => author_mcp(&mut reg),
            "list" => {
                list(&reg);
                Ok(())
            }
            _ => break,
        };
        if let Err(e) = result {
            // A cancelled sub-prompt or a bad file path shouldn't kill the session.
            eprintln!("  ! {e:#}");
        }
    }
    println!("registry saved at {}", dir.display());
    Ok(())
}

fn flag_value(args: &[String], flag: &str) -> Option<String> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned()
}

// ---------------------------------------------------------------------------------------------
// Tests — the authored bytes must pass the REAL agent-side verifier
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use clank_shell::grease::pkg::{verify_inclusion_proof, verify_signature};

    fn temp_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "grease-populate-test-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// Author a package via `write_package`, then verify its index entry with the SAME functions the
    /// durable agent uses — signature over the payload, and the single-leaf RFC-6962 inclusion proof.
    fn assert_authored_ok(reg: &Registry, name: &str, ext: &str, payload: &[u8]) {
        // Served bytes == the bytes we hashed/signed.
        let served = std::fs::read(reg.dir.join("packages").join(format!("{name}.{ext}"))).unwrap();
        assert_eq!(served, payload, "served payload must be byte-identical");

        let index: Value =
            serde_json::from_slice(&std::fs::read(reg.dir.join("index.json")).unwrap()).unwrap();
        let entry = index["packages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["name"] == json!(name))
            .expect("entry present");

        // Content hash matches.
        assert_eq!(entry["sha256"].as_str().unwrap(), sha256_hex(payload));

        // Signature verifies against the tool's public key (verify_strict).
        let pubkey = reg.pubkey_b64();
        verify_signature(payload, entry["sig"].as_str().unwrap(), &pubkey)
            .expect("signature must verify");

        // Single-leaf inclusion proof verifies (leaf = sha256-hex bytes, tree-size 1, empty proof).
        let sha = sha256_hex(payload);
        let root = b64_decode(entry["log"]["root"].as_str().unwrap()).unwrap();
        verify_inclusion_proof(sha.as_bytes(), 0, 1, &root, &[]).expect("log proof must verify");
    }

    #[test]
    fn every_kind_round_trips_through_the_real_verifier() {
        let dir = temp_dir("allkinds");
        let mut reg = Registry::open(&dir, "test-signer").unwrap();

        // prompt (json)
        let p = PromptPackage {
            name: "hello".into(),
            description: "a signed prompt".into(),
            model: None,
            arguments: vec![],
            body: "Say hello.".into(),
        };
        let payload = payload_with_kind(&p, "prompt").unwrap();
        reg.write_package("prompt", "hello", "a signed prompt", &payload, "json").unwrap();
        assert_authored_ok(&reg, "hello", "json", &payload);

        // prompt (.md frontmatter, raw bytes)
        let md = b"---\nname: greeting\ndescription: hi\n---\nSay hi to {{who}}.\n";
        reg.write_package("prompt", "greeting", "hi", md, "md").unwrap();
        assert_authored_ok(&reg, "greeting", "md", md);

        // script
        let s = ScriptPackage {
            name: "hostinfo".into(),
            description: "print hostname".into(),
            arguments: vec![PackageArg {
                name: "label".into(),
                description: "a label".into(),
                required: true,
                default: None,
            }],
            body: "echo {{label}}: $(cat /etc/hostname)".into(),
        };
        let payload = payload_with_kind(&s, "script").unwrap();
        reg.write_package("script", "hostinfo", "print hostname", &payload, "json").unwrap();
        assert_authored_ok(&reg, "hostinfo", "json", &payload);

        // skill
        let sk = SkillPackage {
            name: "reviewing".into(),
            description: "how to review".into(),
            intended_use: Some("when reviewing code".into()),
            documents: vec![SkillDocument {
                path: "SKILL.md".into(),
                content: "Review for correctness first.".into(),
            }],
            scripts: vec![SkillScript {
                name: "note".into(),
                body: "echo check error paths".into(),
            }],
        };
        let payload = payload_with_kind(&sk, "skill").unwrap();
        reg.write_package("skill", "reviewing", "how to review", &payload, "json").unwrap();
        assert_authored_ok(&reg, "reviewing", "json", &payload);

        // agent
        let a = AgentPackage {
            name: "greeter".into(),
            description: "a greeter".into(),
            agent_type: "GreeterAgent".into(),
            constructor_params: vec!["name".into()],
            methods: vec![AgentMethod {
                name: "greet".into(),
                description: "greet someone".into(),
                params: vec!["who".into()],
            }],
            ephemeral: false,
        };
        let payload = payload_with_kind(&a, "agent").unwrap();
        reg.write_package("agent", "greeter", "a greeter", &payload, "json").unwrap();
        assert_authored_ok(&reg, "greeter", "json", &payload);

        // mcp (minimal payload built as a Value)
        let mcp = json!({"kind":"mcp","name":"deepwiki","description":"docs","url":"https://mcp.example/mcp"});
        let payload = serde_json::to_vec_pretty(&mcp).unwrap();
        reg.write_package("mcp", "deepwiki", "docs", &payload, "json").unwrap();
        assert_authored_ok(&reg, "deepwiki", "json", &payload);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reopen_reloads_and_upserts_by_name() {
        let dir = temp_dir("reopen");
        {
            let mut reg = Registry::open(&dir, "s").unwrap();
            let p = PromptPackage {
                name: "hello".into(),
                description: "v1".into(),
                model: None,
                arguments: vec![],
                body: "one".into(),
            };
            let payload = payload_with_kind(&p, "prompt").unwrap();
            reg.write_package("prompt", "hello", "v1", &payload, "json").unwrap();
        }
        // Reopen: existing entry is loaded; re-authoring the same name replaces (not duplicates) it.
        let mut reg = Registry::open(&dir, "s").unwrap();
        assert_eq!(reg.entries.len(), 1);
        let p = PromptPackage {
            name: "hello".into(),
            description: "v2".into(),
            model: None,
            arguments: vec![],
            body: "two".into(),
        };
        let payload = payload_with_kind(&p, "prompt").unwrap();
        reg.write_package("prompt", "hello", "v2", &payload, "json").unwrap();
        assert_eq!(reg.entries.len(), 1, "same name must upsert, not duplicate");
        assert_authored_ok(&reg, "hello", "json", &payload);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The live HTTP server serves the payload + index bytes byte-for-byte (the `grease` fetch path)
    /// and 404s a traversal attempt.
    #[test]
    fn server_serves_registry_files() {
        let dir = temp_dir("serve");
        let mut reg = Registry::open(&dir, "s").unwrap();
        let p = PromptPackage {
            name: "hello".into(),
            description: "d".into(),
            model: None,
            arguments: vec![],
            body: "hi".into(),
        };
        let payload = payload_with_kind(&p, "prompt").unwrap();
        reg.write_package("prompt", "hello", "d", &payload, "json").unwrap();

        let port = serve(dir.clone(), 0).unwrap();

        // GET /packages/hello.json → exact payload bytes.
        let (status, body) = http_get(port, "/packages/hello.json");
        assert!(status.contains("200"), "status: {status}");
        assert_eq!(body, payload, "served payload must be byte-identical");

        // GET /index.json → the on-disk index.
        let (status, body) = http_get(port, "/index.json");
        assert!(status.contains("200"));
        assert_eq!(body, std::fs::read(dir.join("index.json")).unwrap());

        // Traversal is refused.
        let (status, _) = http_get(port, "/../Cargo.toml");
        assert!(status.contains("404"), "traversal must 404: {status}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Minimal HTTP/1.1 GET over a raw TcpStream: returns (status-line, body-bytes).
    fn http_get(port: u16, path: &str) -> (String, Vec<u8>) {
        use std::io::{Read, Write};
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").as_bytes())
            .unwrap();
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).unwrap();
        // Split headers/body on the blank line.
        let sep = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let head = String::from_utf8_lossy(&buf[..sep]).to_string();
        let status = head.lines().next().unwrap_or("").to_string();
        (status, buf[sep + 4..].to_vec())
    }

    /// The persisted signing seed makes the public key stable across reopens (so previously-added
    /// registries keep verifying).
    #[test]
    fn signing_key_is_stable_across_reopen() {
        let dir = temp_dir("key");
        let k1 = Registry::open(&dir, "s").unwrap().pubkey_b64();
        let k2 = Registry::open(&dir, "s").unwrap().pubkey_b64();
        assert_eq!(k1, k2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
