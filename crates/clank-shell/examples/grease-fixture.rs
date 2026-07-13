//! `grease-fixture` — generate a local grease registry for the e2e's `--with-grease` flag.
//!
//! Writes `<dir>/packages/*.json` (one per package kind) and a signed `<dir>/index.json` whose entries
//! carry the real `sha256`, a detached ed25519 `sig`, a `signer`, and an RFC-6962 transparency-log
//! inclusion proof — computed with the SAME code the agent verifies (`clank_shell::grease::pkg`'s
//! `sha256_hex` + the leaf-hash domain separation), so the fixture can never drift from the verifier.
//!
//! Deterministic: the signing key is the fixed `[7u8; 32]` seed (the public key is printed on the last
//! line as `PUBKEY=<base64>` so the e2e can pass it to `grease registry add --key`). Each package is a
//! single-leaf transparency log (tree-size 1, empty proof, root = leaf_hash(sha256-hex)).
//!
//! Usage: `cargo run -q --example grease-fixture -- <output-dir>`

use base64::Engine as _;
use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256};

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// The RFC-6962 leaf hash `sha256(0x00 ‖ data)` — identical to `greasepkg`'s (kept in lockstep).
fn leaf_hash(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update([0x00]);
    h.update(data);
    h.finalize().into()
}

/// One fixture package: its file name, the body served at `/packages/<name>.<ext>`, its extension
/// (`json` for the canonical payload shape, `md` for a frontmatter-authored prompt), and whether it
/// carries a transparency-log proof in the index.
struct Pkg {
    name: &'static str,
    kind: &'static str,
    ext: &'static str,
    body: String,
    with_log: bool,
}

fn main() {
    let dir = std::env::args().nth(1).expect("usage: grease-fixture <output-dir>");
    let pkgdir = std::path::Path::new(&dir).join("packages");
    std::fs::create_dir_all(&pkgdir).expect("create packages dir");

    let sk = SigningKey::from_bytes(&[7u8; 32]);

    // The fixture package set — one per locally-runnable kind. Bodies are compact single-line JSON so
    // the served bytes are exactly what we sign + hash.
    let packages = vec![
        Pkg {
            name: "hello",
            kind: "prompt",
            ext: "json",
            body: r#"{"kind":"prompt","name":"hello","description":"a signed+logged prompt","body":"Say hello."}"#.to_string(),
            with_log: true,
        },
        // A prompt authored as a Markdown file with YAML frontmatter — grease fetches `<name>.md`,
        // verifies integrity over the RAW `.md` bytes, then converts the frontmatter to a PromptPackage.
        Pkg {
            name: "greeting",
            kind: "prompt",
            ext: "md",
            body: "---\nname: greeting\ndescription: a frontmatter-authored prompt\narguments:\n  - name: who\n    required: true\n---\nSay hello to {{who}}.\n".to_string(),
            with_log: false,
        },
        Pkg {
            name: "hostinfo",
            kind: "script",
            ext: "json",
            body: r#"{"kind":"script","name":"hostinfo","description":"print a labelled hostname","arguments":[{"name":"label","required":true,"description":"a label"}],"body":"echo {{label}}: $(cat /etc/hostname)"}"#.to_string(),
            with_log: false,
        },
        Pkg {
            name: "reviewing",
            kind: "skill",
            ext: "json",
            body: r#"{"kind":"skill","name":"reviewing","description":"how to review code","intended-use":"when reviewing code","documents":[{"path":"SKILL.md","content":"Review for correctness first."}],"scripts":[{"name":"review-note","body":"echo check error paths"}]}"#.to_string(),
            with_log: false,
        },
        Pkg {
            name: "cart",
            kind: "agent",
            ext: "json",
            body: r#"{"kind":"agent","name":"cart","description":"a cart agent","agent-type":"ShoppingCart","constructor-params":["userid"],"methods":[{"name":"add-item","description":"add an item","params":["sku"]}],"ephemeral":false}"#.to_string(),
            with_log: false,
        },
    ];

    let mut entries = Vec::new();
    for p in &packages {
        std::fs::write(pkgdir.join(format!("{}.{}", p.name, p.ext)), &p.body).expect("write package");
        let body = p.body.as_bytes();
        let sha = clank_shell::grease::pkg::sha256_hex(body);
        let sig = b64(&sk.sign(body).to_bytes());
        // Single-leaf transparency log: leaf = the sha256-hex string; root = leaf_hash(leaf); proof = [].
        let log = if p.with_log {
            let root = b64(&leaf_hash(sha.as_bytes()));
            format!(
                r#","log":{{"leaf-index":0,"tree-size":1,"root":"{root}","proof":[]}}"#
            )
        } else {
            String::new()
        };
        entries.push(format!(
            r#"{{"name":"{}","kind":"{}","description":"fixture {}","sha256":"{sha}","sig":"{sig}","signer":"clank-fixture"{log}}}"#,
            p.name, p.kind, p.kind
        ));
    }

    let index = format!(r#"{{"packages":[{}]}}"#, entries.join(","));
    std::fs::write(std::path::Path::new(&dir).join("index.json"), index).expect("write index");

    // The e2e reads this to configure `grease registry add --key`.
    println!("PUBKEY={}", b64(&sk.verifying_key().to_bytes()));
}
