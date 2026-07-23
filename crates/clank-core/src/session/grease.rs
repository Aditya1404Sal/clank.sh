//! `Session` methods for the grease package manager: install/remove/list/info/search/update
//! and registry management. Shared install helpers + integrity types live in `super` (mod.rs).

use super::*;

impl Session {
    /// Dispatch a parsed `grease` command.
    pub(super) async fn run_grease(&mut self, cmd: crate::grease::cmd::GreaseCommand) -> LineResult {
        use crate::grease::cmd::GreaseCommand;
        match cmd {
            GreaseCommand::RegistryAdd { url, key } => self.grease_registry_add(&url, key.as_deref()),
            GreaseCommand::RegistryList => self.grease_registry_list(),
            GreaseCommand::RegistryRemove { url } => self.grease_registry_remove(&url),
            GreaseCommand::List => self.grease_list(),
            GreaseCommand::Info { name } => self.grease_info(&name),
            GreaseCommand::Install { name, artifacts } => self.grease_install(&name, artifacts).await,
            GreaseCommand::Remove { name } => self.grease_remove(&name),
            GreaseCommand::Search { query } => self.grease_search(&query).await,
            GreaseCommand::Update { name } => self.grease_update(name.as_deref()).await,
        }
    }

    /// `grease list`: installed packages (all kinds), each tagged with its kind.
    fn grease_list(&self) -> LineResult {
        let packages = self.grease.packages();
        if packages.is_empty() {
            return LineResult::continue_with_stdout(b"no packages installed\n".to_vec());
        }
        let mut out = String::new();
        for p in packages {
            out.push_str(&format!(
                "{}  [{}]  {}\n",
                p.name(),
                p.kind().label(),
                p.payload.description()
            ));
        }
        LineResult::continue_with_stdout(out.into_bytes())
    }

    /// `grease info <name>`: an installed package's metadata. Command packages (prompt/script) show
    /// their generated help; skills (not commands) show the envelope + bundled documents/scripts.
    fn grease_info(&self, name: &str) -> LineResult {
        if let Some(help) = self.grease.pkg_help(name) {
            return LineResult::continue_with_stdout(help.into_bytes());
        }
        if let Some(sk) = self.grease.skill(name) {
            return LineResult::continue_with_stdout(skill_info_text(sk).into_bytes());
        }
        if let Some(m) = self.grease.mcp(name) {
            return LineResult::continue_with_stdout(mcp_info_text(m).into_bytes());
        }
        LineResult::from_outcome(
            Vec::new(),
            format!("grease: '{name}' is not installed\n").into_bytes(),
            1,
        )
    }

    /// `grease install <name>`: fetch the package from the first configured registry that has it,
    /// verify its sha256 (+ signature if the registry is signed), persist it to the store, and register
    /// it. The `artifacts` flags select which of an MCP server's artifact types to install (ignored for
    /// other kinds). A prompt becomes a Confirm command that runs `ask`; a script runs local shell; an
    /// MCP server's tools become `<server> <tool>` commands.
    async fn grease_install(
        &mut self,
        name: &str,
        artifacts: crate::grease::cmd::ArtifactFlags,
    ) -> LineResult {
        if !crate::grease::config::is_valid_name(name) {
            return LineResult::from_outcome(
                Vec::new(),
                format!("grease install: '{name}' is not a valid kebab-case package name\n").into_bytes(),
                2,
            );
        }
        // Reject a name that collides with a static builtin (mirrors `mcp_add`).
        if self.registry.get(name).is_some() {
            return LineResult::from_outcome(
                Vec::new(),
                format!("grease install: '{name}' collides with a built-in command\n").into_bytes(),
                2,
            );
        }
        let registries = crate::grease::config::list_registries();
        if registries.is_empty() {
            return LineResult::from_outcome(
                Vec::new(),
                b"grease install: no registries configured (try `grease registry add <url>`)\n".to_vec(),
                1,
            );
        }
        let Some(http) = self.mcp_http.as_ref() else {
            return LineResult::from_outcome(
                Vec::new(),
                b"grease install: no HTTP transport configured (available on the Golem agent)\n".to_vec(),
                4,
            );
        };

        // Try each registry in order: look up the package's expected sha256 in the registry's
        // index.json, then GET <url>/packages/<name>.json and verify the body matches. The fetch is
        // done here (while `http` is borrowed); persistence happens in `grease_finish_install` (which
        // needs `&mut self`), so we capture the results and drop the borrow first.
        let mut last_err = String::from("package not found in any configured registry");
        // (registry, body, index-entry) captured from the first registry that has the package.
        let mut fetched: Option<(String, Vec<u8>, IndexEntry)> = None;
        'registries: for base in &registries {
            // Integrity metadata from the index (best-effort — a loose registry may omit it).
            let entry = fetch_index_entry(http.as_ref(), base, name).await;
            // Try the JSON payload first, then the `.md` prompt-authoring form. Whichever the registry
            // serves, the raw bytes returned are exactly what integrity is verified over below.
            for ext in ["json", "md"] {
                let url = format!("{}/packages/{name}.{ext}", base.trim_end_matches('/'));
                match http.request("GET", &url, &[], None).await {
                    Ok(resp) if resp.status == 200 => {
                        fetched = Some((base.clone(), resp.body, entry));
                        break 'registries;
                    }
                    Ok(resp) if resp.status == 404 => {
                        last_err = format!("package '{name}' not found (404) at {base}");
                    }
                    Ok(resp) => {
                        last_err = format!("registry {base} returned HTTP {}", resp.status);
                    }
                    Err(e) => {
                        last_err = format!("registry {base}: {e}");
                    }
                }
            }
        }
        let Some((registry, body, entry)) = fetched else {
            return LineResult::from_outcome(
                Vec::new(),
                format!("grease install: {last_err}\n").into_bytes(),
                4,
            );
        };

        // Content-addressed integrity: verify the fetched body against the registry's advertised hash.
        // A mismatch is a hard reject (tamper/corruption). An index that LISTS this package but omits
        // the hash is ALSO a hard reject — a published index must content-address every package, and a
        // missing hash is the exact vector by which a tampered/MITM'd index would bypass verification
        // (README:647 "content-addressed integrity for all package payloads"). Only a registry with no
        // index entry at all (a raw/indexless registry with no integrity claim to check against) falls
        // back to trust-on-first-use, recording the fetched digest for later verification.
        let actual = crate::grease::pkg::sha256_hex(&body);
        let mut note = Vec::new();
        match &entry.sha256 {
            Some(exp) if exp.eq_ignore_ascii_case(&actual) => {} // verified
            Some(exp) => {
                return LineResult::from_outcome(
                    Vec::new(),
                    format!(
                        "grease install: integrity check failed for '{name}': \
                         expected {exp}, got {actual}\n"
                    )
                    .into_bytes(),
                    4,
                );
            }
            None if entry.found_in_index => {
                return LineResult::from_outcome(
                    Vec::new(),
                    format!(
                        "grease install: '{name}' is listed in the {registry} index without a sha256 \
                         — refusing to install (every indexed package must be content-addressed)\n"
                    )
                    .into_bytes(),
                    4,
                );
            }
            None => {
                note.extend_from_slice(
                    format!(
                        "grease: {registry} serves no index for '{name}' — trust-on-first-use, \
                         recording the fetched digest\n"
                    )
                    .as_bytes(),
                );
            }
        }

        // Signature verification: if the registry was configured with a trusted key (`grease registry
        // add --key`), the payload's detached ed25519 signature MUST verify against it. A configured
        // key with a missing/invalid signature is a HARD reject (a signed registry must sign its
        // packages). No configured key ⇒ unsigned registry (record-only, as before), with a note.
        let mut signature_verified = false;
        let mut signer: Option<String> = None;
        if let Some(trusted_key) = crate::grease::config::registry_key(&registry) {
            let Some(sig) = entry.sig.as_deref() else {
                return LineResult::from_outcome(
                    Vec::new(),
                    format!(
                        "grease install: '{name}' has no signature but {registry} is a signed \
                         registry — refusing to install\n"
                    )
                    .into_bytes(),
                    4,
                );
            };
            if let Err(e) = crate::grease::pkg::verify_signature(&body, sig, &trusted_key) {
                return LineResult::from_outcome(
                    Vec::new(),
                    format!("grease install: signature verification failed for '{name}': {e}\n")
                        .into_bytes(),
                    4,
                );
            }
            signature_verified = true;
            signer = entry.signer.clone().or(Some("registry key".to_string()));
        } else {
            note.extend_from_slice(
                format!("grease: {registry} is unsigned (no trusted key) — installing unsigned\n")
                    .as_bytes(),
            );
        }

        // Transparency-log auditing (the other half of README:647): if the registry advertises an
        // RFC-6962 inclusion proof for this package AND the registry is signed (trust is rooted in the
        // same registry key), verify the payload's inclusion in the log against the advertised root. A
        // present-but-invalid proof is a HARD reject; an absent proof leaves the package
        // not-log-audited (as unsigned leaves it unsigned). The root is registry-advertised (not a
        // public witnessed log) — see [[clank-grease]].
        let mut log_verified = false;
        let mut log_index: Option<u64> = None;
        if signature_verified {
            if let Some(log) = &entry.log {
                match verify_log_inclusion(&actual, log) {
                    Ok(()) => {
                        log_verified = true;
                        log_index = Some(log.leaf_index);
                    }
                    Err(e) => {
                        return LineResult::from_outcome(
                            Vec::new(),
                            format!("grease install: transparency-log check failed for '{name}': {e}\n")
                                .into_bytes(),
                            4,
                        );
                    }
                }
            }
        }

        // An MCP-server package needs a live step (initialize + tools/list + prompts/list +
        // resources/list) to enrich its cached surface before persistence — done async here. Other
        // kinds persist synchronously.
        let integrity = InstallIntegrity {
            sha256: actual,
            verified: entry.sha256.is_some(),
            signature_verified,
            signer,
            log_verified,
            log_index,
        };

        // A prompt authored as Markdown (leading `---` frontmatter) is converted to the canonical prompt
        // JSON shape here — AFTER integrity was verified over the raw `.md` bytes (above), so the store
        // and boot path (`load_one`) stay JSON-uniform and never need `.md` awareness. Integrity is NOT
        // re-checked against the converted JSON: the served `.md` bytes are what the registry signed/
        // logged. A JSON body passes through untouched.
        let body = if is_markdown_frontmatter(&body) {
            match crate::grease::pkg::PromptPackage::from_markdown(&body) {
                Ok(p) => p.to_json().into_bytes(),
                Err(e) => {
                    return LineResult::from_outcome(
                        Vec::new(),
                        format!("grease install: {e}\n").into_bytes(),
                        4,
                    )
                }
            }
        } else {
            body
        };

        if crate::grease::pkg::payload_kind(&body) == Ok(crate::grease::pkg::PackageKind::Mcp) {
            return self.grease_finish_install_mcp(name, &registry, &body, integrity, artifacts, note).await;
        }

        self.grease_finish_install(name, &registry, &body, integrity, note)
    }

    /// The MCP-server install path: parse the minimal payload, fetch the live artifact surface
    /// (tools/prompts, and resources if selected), enrich the payload with the cached listings,
    /// persist it, register the server into `McpState` (so its tools become `<server> <tool>`
    /// commands), materialize any prompts as standalone prompt packages, materialize static resources,
    /// and write the marker. Reuses the existing `mcp_install` machinery for tool registration.
    async fn grease_finish_install_mcp(
        &mut self,
        name: &str,
        registry: &str,
        body: &[u8],
        integrity: InstallIntegrity,
        artifacts: crate::grease::cmd::ArtifactFlags,
        mut note: Vec<u8>,
    ) -> LineResult {
        // Parse + name-check the minimal registry payload.
        let mut pkg = match crate::grease::pkg::McpPackage::from_json(body) {
            Ok(p) => p,
            Err(e) => return LineResult::from_outcome(Vec::new(), format!("grease install: {e}\n").into_bytes(), 4),
        };
        if pkg.name != name {
            return LineResult::from_outcome(
                Vec::new(),
                format!("grease install: registry returned package '{}' for request '{name}'\n", pkg.name).into_bytes(),
                4,
            );
        }
        // The install-line flags select which artifact types to expose (no flags = all three).
        pkg.artifacts =
            crate::grease::pkg::McpArtifacts::from_flags(artifacts.tools, artifacts.prompts, artifacts.resources);

        let Some(http) = self.mcp_http.as_deref() else {
            return LineResult::from_outcome(
                Vec::new(),
                b"grease install: no HTTP transport configured (available on the Golem agent)\n".to_vec(),
                4,
            );
        };

        // Build an MCP client against the server and initialize once.
        let config = crate::mcp::config::McpServerConfig {
            url: pkg.url.clone(),
            enabled: true,
            auth_env: pkg.auth_env.clone(),
            auth_header: None,
            tools: Vec::new(),
        };
        let auth = config.resolve_auth();
        let mut client = crate::mcp::client::McpClient::new(http, &pkg.url, auth);
        let init = match client.initialize().await {
            Ok(i) => i,
            Err(e) => {
                return LineResult::from_outcome(
                    Vec::new(),
                    format!("grease install: {name}: {}\n", e.message).into_bytes(),
                    e.exit_code,
                )
            }
        };
        let session = init.session_id.clone();

        // Fetch the selected artifact surfaces. tools/list is required when --tools; prompts/list and
        // resources/list are best-effort (a server may not support them → treated as empty).
        let mut tool_specs = Vec::new();
        if pkg.artifacts.tools {
            match client.list_tools(session.as_deref()).await {
                Ok(t) => tool_specs = t,
                Err(e) => {
                    return LineResult::from_outcome(
                        Vec::new(),
                        format!("grease install: {name}: tools/list: {}\n", e.message).into_bytes(),
                        e.exit_code,
                    )
                }
            }
        }
        let prompt_specs = if pkg.artifacts.prompts {
            client.list_prompts(session.as_deref()).await.unwrap_or_default()
        } else {
            Vec::new()
        };

        // Cache the tool + prompt listings in the payload (so `load()` rebuilds offline).
        pkg.tools = tool_specs
            .iter()
            .map(|t| crate::grease::pkg::McpToolCache {
                name: t.name.clone(),
                description: t.description.clone().unwrap_or_default(),
                input_schema: t.input_schema.to_string(),
            })
            .collect();
        pkg.prompts = prompt_specs
            .iter()
            .map(|p| crate::grease::pkg::McpPromptCache {
                name: p.name.clone(),
                description: p.description.clone().unwrap_or_default(),
            })
            .collect();

        // Materialize any prompts as standalone prompt packages (fetch each body via prompts/get).
        let mut installed_prompts = Vec::new();
        for p in &prompt_specs {
            let args = serde_json::json!({});
            // A failed `prompts/get` must NOT be coerced to an empty body — that would persist an
            // empty prompt package and the install would look successful, so the user later runs the
            // prompt and gets a silently empty result. Skip the prompt and note it (audit P3-4). A
            // legitimately empty body (`Ok("")`) still installs.
            match client.get_prompt(&p.name, args, session.as_deref()).await {
                Ok(body_text) => installed_prompts.push((p.clone(), body_text)),
                Err(e) => note.extend_from_slice(
                    format!("grease: skipping prompt '{}' — fetch failed: {e:?}\n", p.name).as_bytes(),
                ),
            }
        }

        // Materialize selected resources (static files under /mnt/mcp/<server>/) + resource templates
        // (executables in /usr/lib/mcp/bin). Best-effort.
        if pkg.artifacts.resources {
            pkg.resources = materialize_mcp_resources(name, &mut client, session.as_deref()).await;
            // Templates: fetch `resources/templates/list` and cache as `<server>-<tname>` executables.
            let templates = client.list_resource_templates(session.as_deref()).await.unwrap_or_default();
            pkg.templates = templates
                .iter()
                .filter_map(|t| {
                    let tname = t.name.clone()?;
                    let cmd = format!("{name}-{tname}");
                    if !crate::grease::config::is_valid_name(&cmd) {
                        return None;
                    }
                    Some(crate::grease::pkg::McpTemplateCache {
                        name: cmd,
                        uri_template: t.uri_template.clone(),
                        description: t.description.clone().unwrap_or_default(),
                    })
                })
                .collect();
        }
        let resource_count = pkg.resources.len();
        let template_count = pkg.templates.len();

        // Persist the enriched payload + marker.
        let payload = crate::grease::state::Payload::Mcp(pkg.clone());
        if let Err(msg) = self.persist_package(name, crate::grease::pkg::PackageKind::Mcp, &payload) {
            return LineResult::from_outcome(Vec::new(), msg.into_bytes(), 1);
        }
        let marker = integrity.to_marker(crate::grease::pkg::PackageKind::Mcp, registry);
        if let Err(msg) = write_install_marker(name, &marker) {
            return LineResult::from_outcome(Vec::new(), msg.into_bytes(), 1);
        }

        // Register the server + tools into `McpState` (so `<server> <tool>` dispatch + the mcp bin stub
        // work), reusing the mcp machinery.
        let tool_count = tool_specs.len();
        if pkg.artifacts.tools {
            let mcp_tools: Vec<crate::mcp::state::McpTool> = tool_specs.into_iter().map(Into::into).collect();
            self.mcp.set_installed(name, config, mcp_tools);
            if let Some(help) = self.mcp.server_help(name) {
                let _ = crate::mcp::config::write_bin_stub(name, &help);
            }
        }

        // Materialize the prompt packages (each becomes an installed grease prompt on $PATH).
        let prompt_count = installed_prompts.len();
        for (spec, body_text) in installed_prompts {
            self.install_mcp_prompt(&spec, &body_text, registry);
        }

        // Write a /usr/lib/mcp/bin stub for each resource-template executable (so which/type/ls see it).
        for t in &pkg.templates {
            let help = format!(
                "{} — MCP resource template ({}). Run `{} <arg…>` to read the constructed URI.\n",
                t.name, t.uri_template, t.name
            );
            let _ = crate::mcp::config::write_bin_stub(&t.name, &help);
        }

        // Register the grease package view.
        self.grease.set_installed(crate::grease::state::InstalledPackage { marker, payload });

        note.extend_from_slice(
            format!(
                "installed {name} [mcp] ({})\n\
                 {tool_count} tools, {prompt_count} prompts, {resource_count} resources, \
                 {template_count} templates\n\
                 tools run as `{name} <tool>`\n",
                integrity.summary()
            )
            .as_bytes(),
        );
        LineResult::continue_with_stdout(note)
    }

    /// Verify + persist a fetched package payload and register it, dispatching on the payload's
    /// declared `kind`. `sha256` is the (already-computed) digest of `body`; `verified` marks whether
    /// it matched the registry's advertised hash; `signature_verified`/`signer` record the ed25519
    /// signing status resolved in `grease_install`.
    fn grease_finish_install(
        &mut self,
        name: &str,
        registry: &str,
        body: &[u8],
        integrity: InstallIntegrity,
        note: Vec<u8>,
    ) -> LineResult {
        let kind = match crate::grease::pkg::payload_kind(body) {
            Ok(k) => k,
            Err(e) => {
                return LineResult::from_outcome(
                    Vec::new(),
                    format!("grease install: {e}\n").into_bytes(),
                    4,
                )
            }
        };
        // Parse the payload for this kind and confirm its own name matches the request (guards a
        // misconfigured registry).
        let payload = match self.parse_and_check_payload(name, kind, body) {
            Ok(p) => p,
            Err(msg) => return LineResult::from_outcome(Vec::new(), msg.into_bytes(), 4),
        };

        // Persist the typed payload + write the marker + materialize the kind's on-disk surface.
        if let Err(msg) = self.persist_package(name, kind, &payload) {
            return LineResult::from_outcome(Vec::new(), msg.into_bytes(), 1);
        }
        let marker = integrity.to_marker(kind, registry);
        if let Err(msg) = write_install_marker(name, &marker) {
            return LineResult::from_outcome(Vec::new(), msg.into_bytes(), 1);
        }
        let installed = crate::grease::state::InstalledPackage { marker, payload };
        // Materialize the kind's on-disk surface (bin stub / skill dir tree) — needs the help text,
        // which is derived from the registered package, so register first.
        self.grease.set_installed(installed);
        self.materialize_package(name, kind);

        let run_hint = match kind {
            crate::grease::pkg::PackageKind::Skill => {
                format!("see it with `grease info {name}`")
            }
            _ => format!("run it with `{name}`"),
        };
        let mut out = note; // any record-only/unsigned note first
        out.extend_from_slice(
            format!(
                "installed {name} [{}] ({})\n{run_hint}\n",
                kind.label(),
                integrity.summary()
            )
            .as_bytes(),
        );
        LineResult::continue_with_stdout(out)
    }

    /// Install an MCP server's prompt as a standalone grease prompt package (README: MCP prompts are
    /// installed to `/usr/lib/prompts/bin` and are indistinguishable from standalone prompts). The
    /// prompt's declared arguments become the package arguments; `{{arg}}` placeholders in the fetched
    /// body are already resolved server-side for the empty-arg fetch, so v1 stores the fetched body as
    /// a non-parameterized prompt (re-fetch with args is a future refinement).
    fn install_mcp_prompt(&mut self, spec: &crate::mcp::client::PromptSpec, body: &str, registry: &str) {
        let pkg = crate::grease::pkg::PromptPackage {
            name: spec.name.clone(),
            description: spec.description.clone().unwrap_or_default(),
            model: None,
            arguments: Vec::new(),
            body: body.to_string(),
        };
        // Persist as a prompt package + marker + bin stub, and register it.
        let payload = crate::grease::state::Payload::Prompt(pkg);
        if self.persist_package(&spec.name, crate::grease::pkg::PackageKind::Prompt, &payload).is_err() {
            return;
        }
        let sha = crate::grease::pkg::sha256_hex(body.as_bytes());
        let marker = crate::grease::state::InstallMarker {
            kind: crate::grease::pkg::PackageKind::Prompt,
            registry: registry.to_string(),
            sha256: sha,
            verified: false,
            signature_verified: false,
            signer: None,
            log_verified: false,
            log_index: None,
        };
        if write_install_marker(&spec.name, &marker).is_err() {
            return;
        }
        self.grease.set_installed(crate::grease::state::InstalledPackage { marker, payload });
        self.materialize_package(&spec.name, crate::grease::pkg::PackageKind::Prompt);
    }

    /// Parse a fetched payload for `kind` into a [`crate::grease::state::Payload`], verifying the
    /// package's own name matches the requested `name`. Returns an error string on parse/name mismatch.
    fn parse_and_check_payload(
        &self,
        name: &str,
        kind: crate::grease::pkg::PackageKind,
        body: &[u8],
    ) -> Result<crate::grease::state::Payload, String> {
        use crate::grease::pkg::{
            AgentPackage, McpPackage, PackageKind, PromptPackage, ScriptPackage, SkillPackage,
        };
        use crate::grease::state::Payload;
        let (payload, pkg_name) = match kind {
            PackageKind::Prompt => {
                let p = PromptPackage::from_json(body).map_err(|e| format!("grease install: {e}\n"))?;
                let n = p.name.clone();
                (Payload::Prompt(p), n)
            }
            PackageKind::Script => {
                let s = ScriptPackage::from_json(body).map_err(|e| format!("grease install: {e}\n"))?;
                let n = s.name.clone();
                (Payload::Script(s), n)
            }
            PackageKind::Skill => {
                let s = SkillPackage::from_json(body).map_err(|e| format!("grease install: {e}\n"))?;
                let n = s.name.clone();
                (Payload::Skill(s), n)
            }
            PackageKind::Mcp => {
                let m = McpPackage::from_json(body).map_err(|e| format!("grease install: {e}\n"))?;
                let n = m.name.clone();
                (Payload::Mcp(m), n)
            }
            PackageKind::Agent => {
                let a = AgentPackage::from_json(body).map_err(|e| format!("grease install: {e}\n"))?;
                let n = a.name.clone();
                (Payload::Agent(a), n)
            }
        };
        if pkg_name != name {
            return Err(format!(
                "grease install: registry returned package '{pkg_name}' for request '{name}'\n"
            ));
        }
        Ok(payload)
    }

    /// Persist a typed payload to `<store>/<name>/<kind>.json`.
    fn persist_package(
        &self,
        name: &str,
        kind: crate::grease::pkg::PackageKind,
        payload: &crate::grease::state::Payload,
    ) -> Result<(), String> {
        use crate::grease::state::Payload;
        let store = crate::grease::config::store_dir().join(name);
        std::fs::create_dir_all(&store)
            .map_err(|e| format!("grease install: cannot create store dir: {e}\n"))?;
        let json = match payload {
            Payload::Prompt(p) => p.to_json(),
            Payload::Script(s) => s.to_json(),
            Payload::Skill(s) => s.to_json(),
            Payload::Mcp(m) => m.to_json(),
            Payload::Agent(a) => a.to_json(),
        };
        std::fs::write(store.join(kind.payload_file()), json)
            .map_err(|e| format!("grease install: cannot write payload: {e}\n"))
    }

    /// Materialize a kind's on-disk surface after registration: a bin stub for command packages
    /// (prompt→`/usr/lib/prompts/bin`, script→`/usr/bin`) or the skill dir tree (docs + bundled
    /// `bin/` scripts) for a skill. Best-effort — the durable payload is already persisted.
    fn materialize_package(&self, name: &str, kind: crate::grease::pkg::PackageKind) {
        use crate::grease::pkg::PackageKind;
        match kind {
            PackageKind::Prompt => {
                let help = self
                    .grease
                    .pkg_help(name)
                    .unwrap_or_else(|| format!("{name} — installed prompt\n"));
                let _ = crate::grease::config::write_bin_stub(
                    &crate::grease::config::bin_dir(),
                    name,
                    &help,
                    "prompt",
                );
            }
            PackageKind::Script => {
                let help = self
                    .grease
                    .pkg_help(name)
                    .unwrap_or_else(|| format!("{name} — installed script\n"));
                let _ = crate::grease::config::write_bin_stub(
                    &crate::grease::config::script_bin_dir(),
                    name,
                    &help,
                    "script",
                );
            }
            PackageKind::Skill => {
                if let Some(sk) = self.grease.skill(name) {
                    let _ = crate::grease::config::materialize_skill(sk);
                }
            }
            PackageKind::Mcp => {
                // MCP registration into `McpState` (tools) + prompt materialization happens in the
                // async `grease_finish_install_mcp` path (it needs the live server); nothing to do
                // synchronously here.
            }
            PackageKind::Agent => {
                let help = self
                    .grease
                    .pkg_help(name)
                    .unwrap_or_else(|| format!("{name} — installed agent\n"));
                let _ = crate::grease::config::write_bin_stub(
                    &crate::grease::config::agent_bin_dir(),
                    name,
                    &help,
                    "agent",
                );
            }
        }
    }

    /// `grease remove <name>`: delete the store, marker, and the kind's on-disk surface, and
    /// deregister.
    fn grease_remove(&mut self, name: &str) -> LineResult {
        let Some(kind) = self.grease.kind_of(name) else {
            return LineResult::from_outcome(
                Vec::new(),
                format!("grease remove: '{name}' is not installed\n").into_bytes(),
                1,
            );
        };
        let _ = std::fs::remove_file(crate::grease::config::etc_dir().join(format!("{name}.toml")));
        let _ = std::fs::remove_dir_all(crate::grease::config::store_dir().join(name));
        match kind {
            crate::grease::pkg::PackageKind::Prompt => {
                let _ = std::fs::remove_file(crate::grease::config::bin_dir().join(name));
            }
            crate::grease::pkg::PackageKind::Script => {
                let _ = std::fs::remove_file(crate::grease::config::script_bin_dir().join(name));
            }
            crate::grease::pkg::PackageKind::Skill => {
                let _ = std::fs::remove_dir_all(crate::grease::config::skills_dir().join(name));
            }
            crate::grease::pkg::PackageKind::Mcp => {
                // Deregister the server from `McpState` (also removes its /usr/lib/mcp/bin stub) and
                // remove any materialized resource tree under /mnt/mcp/<name>/.
                let _ = crate::mcp::config::remove(name);
                self.mcp.remove(name);
                let _ = std::fs::remove_dir_all(crate::grease::config::mcp_mount_dir().join(name));
            }
            crate::grease::pkg::PackageKind::Agent => {
                let _ = std::fs::remove_file(crate::grease::config::agent_bin_dir().join(name));
            }
        }
        self.grease.remove(name);
        LineResult::continue_with_stdout(format!("removed {name}\n").into_bytes())
    }

    /// `grease search <query>`: fetch each registry's `index.json` and list matching package names.
    async fn grease_search(&mut self, query: &str) -> LineResult {
        let registries = crate::grease::config::list_registries();
        if registries.is_empty() {
            return LineResult::from_outcome(
                Vec::new(),
                b"grease search: no registries configured\n".to_vec(),
                1,
            );
        }
        let Some(http) = self.mcp_http.as_ref() else {
            return LineResult::from_outcome(
                Vec::new(),
                b"grease search: no HTTP transport configured (available on the Golem agent)\n".to_vec(),
                4,
            );
        };
        let mut hits = Vec::new();
        for base in &registries {
            let url = format!("{}/index.json", base.trim_end_matches('/'));
            if let Ok(resp) = http.request("GET", &url, &[], None).await {
                if resp.status == 200 {
                    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&resp.body) {
                        if let Some(arr) = v.get("packages").and_then(|p| p.as_array()) {
                            for pkg in arr {
                                let name = pkg.get("name").and_then(|n| n.as_str()).unwrap_or("");
                                let desc = pkg.get("description").and_then(|d| d.as_str()).unwrap_or("");
                                let kind = pkg.get("kind").and_then(|k| k.as_str()).unwrap_or("prompt");
                                if name.contains(query) || desc.contains(query) {
                                    hits.push(format!("{name}  [{kind}]  {desc}"));
                                }
                            }
                        }
                    }
                }
            }
        }
        if hits.is_empty() {
            return LineResult::continue_with_stdout(format!("no packages match '{query}'\n").into_bytes());
        }
        hits.sort();
        hits.dedup();
        LineResult::continue_with_stdout(format!("{}\n", hits.join("\n")).into_bytes())
    }

    /// `grease update [<name>]`: re-fetch + re-verify + re-persist installed packages (all, or one).
    async fn grease_update(&mut self, name: Option<&str>) -> LineResult {
        let targets: Vec<String> = match name {
            Some(n) if self.grease.get(n).is_some() => vec![n.to_string()],
            Some(n) => {
                return LineResult::from_outcome(
                    Vec::new(),
                    format!("grease update: '{n}' is not installed\n").into_bytes(),
                    1,
                )
            }
            None => self.grease.packages().iter().map(|p| p.name().to_string()).collect(),
        };
        if targets.is_empty() {
            return LineResult::continue_with_stdout(b"nothing to update\n".to_vec());
        }
        let mut out = String::new();
        for t in targets {
            // Re-install preserving the package's existing artifact selection (for MCP; a no-op for
            // other kinds). The stored payload carries the prior `artifacts`, so pass its flags.
            let flags = self
                .grease
                .mcp(&t)
                .map(|m| crate::grease::cmd::ArtifactFlags {
                    tools: m.artifacts.tools,
                    prompts: m.artifacts.prompts,
                    resources: m.artifacts.resources,
                })
                .unwrap_or_default();
            let result = Box::pin(self.grease_install(&t, flags)).await;
            out.push_str(&String::from_utf8_lossy(&result.terminal_output()));
        }
        LineResult::continue_with_stdout(out.into_bytes())
    }

    /// `grease registry add <url> [--key <base64-ed25519-pubkey>]`: record a registry URL and, if
    /// given, its trusted signing key. The key is validated (must decode to a 32-byte ed25519 key)
    /// before it's stored, so a typo is caught at `add` time, not at install time.
    fn grease_registry_add(&self, url: &str, key: Option<&str>) -> LineResult {
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return LineResult::from_outcome(
                Vec::new(),
                format!("grease registry add: '{url}' is not an http(s) URL\n").into_bytes(),
                2,
            );
        }
        if let Some(k) = key {
            if let Err(e) = crate::grease::pkg::validate_public_key(k) {
                return LineResult::from_outcome(
                    Vec::new(),
                    format!("grease registry add: {e}\n").into_bytes(),
                    2,
                );
            }
        }
        match crate::grease::config::add_registry(url, key) {
            Ok(true) => {
                let msg = if key.is_some() {
                    format!("added registry {url} (signed)\n")
                } else {
                    format!("added registry {url}\n")
                };
                LineResult::continue_with_stdout(msg.into_bytes())
            }
            Ok(false) => {
                LineResult::continue_with_stdout(format!("registry {url} already present\n").into_bytes())
            }
            Err(e) => LineResult::from_outcome(Vec::new(), format!("grease: {e}\n").into_bytes(), 1),
        }
    }

    /// `grease registry list`: the configured registry URLs.
    fn grease_registry_list(&self) -> LineResult {
        let urls = crate::grease::config::list_registries();
        if urls.is_empty() {
            return LineResult::continue_with_stdout(b"no registries configured\n".to_vec());
        }
        LineResult::continue_with_stdout(format!("{}\n", urls.join("\n")).into_bytes())
    }

    /// `grease registry remove <url>`: drop a registry URL.
    fn grease_registry_remove(&self, url: &str) -> LineResult {
        match crate::grease::config::remove_registry(url) {
            Ok(true) => LineResult::continue_with_stdout(format!("removed registry {url}\n").into_bytes()),
            Ok(false) => LineResult::from_outcome(
                Vec::new(),
                format!("grease: registry {url} was not configured\n").into_bytes(),
                1,
            ),
            Err(e) => LineResult::from_outcome(Vec::new(), format!("grease: {e}\n").into_bytes(), 1),
        }
    }

}
