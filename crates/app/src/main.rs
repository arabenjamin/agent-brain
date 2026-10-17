#[cfg(feature = "http-transport")]
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use tracing::{error, info, warn};

use agent_brain::cli::{Cli, Command, TodoAction, TransportType};
use agent_brain::config::{Config, LogFormat, LoggingConfig};
use agent_brain::logging;
use agent_brain::mcp::McpServer;
#[cfg(feature = "http-transport")]
use agent_brain::mcp::{HttpTransport, HttpTransportConfig, McpServerCore};
use agent_brain::repository::Neo4jClient;
use agent_brain::services::{LlmConfig, ModelCatalog};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env file if present
    dotenvy::dotenv().ok();

    // Parse CLI arguments
    let cli = Cli::parse();

    // Build config from environment and override with CLI args
    let mut config = Config::from_env()?;

    config.database.uri = cli.neo4j_uri.clone();
    config.database.user = cli.neo4j_user.clone();
    if let Some(pw) = &cli.neo4j_password {
        config.database.password = pw.clone();
    }
    config.logging = LoggingConfig {
        level: cli.log_level.clone(),
        format: match cli.log_format.to_lowercase().as_str() {
            "json" => LogFormat::Json,
            _ => LogFormat::Pretty,
        },
    };

    // Create log ring buffer and initialize logging.
    // The buffer is passed through to the HTTP transport for GET /api/logs.
    // For stdio transport it's unused but we create it anyway to avoid cfg complexity.
    // 2,000 lines: at a few lines per scheduler tick plus job lines, 500 held
    // only about an hour, too short to see what happened overnight.
    let log_buffer = agent_brain::logging::LogBuffer::new(2000);
    logging::init_with_buffer(&config.logging, Some(log_buffer.clone()));

    info!(
        neo4j_uri = %config.database.uri,
        "Starting agent-brain"
    );

    // Report the zone every dated statement will be made in. An accidental UTC
    // is indistinguishable from a deliberate one at every later call site, so
    // it gets named once, here, where it can be checked.
    agent_brain::services::clock::log_resolved_timezone();

    // Execute command
    let result = match cli.command {
        Some(Command::InitDb) => run_init_db(&config).await,
        Some(Command::Serve {
            transport,
            bind,
            api_key,
        }) => run_serve(&config, transport, &bind, api_key, log_buffer).await,
        Some(Command::Todo { action, url }) => run_todo(&url, action).await,
        Some(Command::RepairNotes { dry_run }) => run_repair_notes(&config, dry_run).await,
        Some(Command::ResolveEntities { dry_run, unmerge }) => {
            run_resolve_entities(&config, dry_run, unmerge.as_deref()).await
        }
        Some(Command::RestoreSnapshot { path, dry_run }) => {
            run_restore_snapshot(&config, &path, dry_run).await
        }
        Some(Command::EvalRetrieval {
            fixture,
            k,
            bootstrap,
        }) => run_eval_retrieval(&config, &fixture, k, bootstrap).await,
        None => {
            // Default to stdio transport when no command specified
            run_serve(
                &config,
                TransportType::Stdio,
                "127.0.0.1:3000",
                None,
                log_buffer,
            )
            .await
        }
    };

    if let Err(e) = &result {
        error!(error = %e, "Command failed");
    }

    result
}

async fn connect_neo4j(config: &Config) -> Result<Neo4jClient> {
    info!("Connecting to Neo4j...");
    let client = Neo4jClient::new(
        &config.database.uri,
        &config.database.user,
        &config.database.password,
    )
    .await?;
    info!("Connected to Neo4j");
    warn_on_string_timestamps(&client).await;
    Ok(client)
}

/// Warn when any temporal property is still stored as a string.
///
/// Neo4j does not enforce property types, so one missed `SET` silently
/// reintroduces the mixed-representation split that made date filters match
/// nothing without erroring. Like the timezone check above, this gets named
/// once at startup where it can be acted on — at any later call site an empty
/// result set is indistinguishable from an honest one.
///
/// Deliberately non-fatal: the reads tolerate both representations, so a
/// violation degrades comparisons rather than breaking startup.
async fn warn_on_string_timestamps(client: &Neo4jClient) {
    match client.string_timestamp_violations().await {
        Ok(v) if v.is_empty() => {}
        Ok(violations) => {
            let total: i64 = violations.iter().map(|(_, _, c)| c).sum();
            let detail = violations
                .iter()
                .map(|(label, prop, count)| format!("{label}.{prop} ({count})"))
                .collect::<Vec<_>>()
                .join(", ");
            warn!(
                total,
                properties = %detail,
                "Temporal properties are stored as STRING, not DATETIME. Cypher \
                 compares a datetime to a string as null, so date filters over \
                 these will silently match nothing. Run \
                 scripts/migrate_timestamps_to_datetime.cypher"
            );
        }
        Err(e) => warn!(error = %e, "Could not check timestamp property types"),
    }
}

fn build_llm_config(config: &Config) -> LlmConfig {
    config_for_active(config, &config.llm.provider, &config.llm.model)
}

/// The config for `provider` + `model`, built the way every routed config is
/// (`model_router::config_for_catalog_entry`: endpoint and key from the
/// provider registry) on a base that pins embeddings to host Ollama.
fn config_for_active(config: &Config, provider: &str, model: &str) -> LlmConfig {
    let llm = &config.llm;
    let mut base = LlmConfig::default().with_embed_base_url(llm.ollama_local_url.clone());
    if let Some(embed_model) = &llm.ollama_embed_model {
        base = base.with_embed_model(embed_model);
    }
    // Applied for every provider: the embedding endpoint is always local Ollama,
    // so even an Anthropic/Gemini deployment has a local model worth keeping
    // resident. Non-Ollama providers never serialize the field.
    if let Some(ka) = &llm.ollama_keep_alive {
        base = base.with_keep_alive(ka);
    }
    agent_brain::services::model_router::config_for_catalog_entry(&base, provider, model)
}

/// Build the LLM config for the human-facing chat adapter.
///
/// Starts from the brain's LLM config and applies any `CHAT_LLM_*` overrides
/// from [`Config::chat_llm`].  When all overrides are `None` the result is
/// identical to the brain's config (single-model deployments are unchanged).
#[cfg(feature = "http-transport")]
fn build_chat_llm_config(config: &Config) -> LlmConfig {
    let chat = &config.chat_llm;

    // No overrides at all → return the exact same config as the brain.
    if chat.provider.is_none()
        && chat.model.is_none()
        && chat.api_key.is_none()
        && chat.base_url.is_none()
    {
        return build_llm_config(config);
    }

    // A provider override is a different registry entry (its endpoint and
    // key); a model override alone keeps the brain's provider.
    let provider = chat.provider.as_deref().unwrap_or(&config.llm.provider);
    let model = match (&chat.model, &chat.provider) {
        (Some(m), _) => m.clone(),
        (None, Some(p)) => agent_brain::config::default_model_for(p),
        (None, None) => config.llm.model.clone(),
    };
    let mut base = config_for_active(config, provider, &model);
    if let Some(ref key) = chat.api_key {
        base = base.with_api_key(key);
    }
    if let Some(ref url) = chat.base_url {
        base = base.with_base_url(url.clone());
    }
    base
}

/// One-off repair of notes that persisted a query envelope instead of its content.
///
/// Runs standalone (no MCP server) so it can be executed against a live deployment
/// with the scheduler paused. Embeddings route through the same LLM config the
/// server uses, so vectors are computed with the configured embed model.
async fn run_restore_snapshot(config: &Config, path: &str, dry_run: bool) -> Result<()> {
    let client = connect_neo4j(config).await?;
    let svc = agent_brain::services::SnapshotService::new(client, std::path::PathBuf::new());
    let stats = svc
        .restore_snapshot(std::path::Path::new(path), dry_run)
        .await?;
    println!("{}", serde_json::to_string_pretty(&stats)?);
    Ok(())
}

async fn run_repair_notes(config: &Config, dry_run: bool) -> Result<()> {
    use agent_brain::services::repair;
    use std::sync::Arc;

    let client = connect_neo4j(config).await?;
    // SharedLlm is the LlmProvider impl; it reads a live config handle, so wrap
    // the static config the same way the server does.
    let llm_config = build_llm_config(config);
    let shared = Arc::new(tokio::sync::RwLock::new(Some(llm_config)));
    let llm: Arc<dyn agent_brain::services::traits::LlmProvider> =
        agent_brain::services::shared_llm::SharedLlm::new(shared);
    let knowledge =
        agent_brain::services::knowledge::KnowledgeService::new(client.clone(), Some(llm));

    if dry_run {
        info!("Repair running in DRY-RUN mode — nothing will be written");
    }
    let stats = repair::repair_envelope_notes(&client, &knowledge, dry_run).await?;

    info!(
        examined = stats.examined,
        repaired = stats.repaired,
        skipped = stats.skipped_unparseable,
        chunks_removed = stats.chunks_removed,
        chunks_created = stats.chunks_created,
        "Repair finished"
    );
    if !dry_run && stats.repaired > 0 {
        // Entity extraction is spawned per note and outlives the repair loop.
        info!("Waiting for background entity extraction to drain...");
        tokio::time::sleep(std::time::Duration::from_secs(20)).await;
    }
    Ok(())
}

/// Score retrieval against a golden set, or bootstrap proposed cases.
///
/// Standalone like `run_repair_notes` so it runs against a live deployment
/// without starting the MCP server. Uses the same LLM config the server uses,
/// so query embeddings are computed with the configured embed model — the
/// point of the harness is to measure the *real* pipeline, not a proxy.
async fn run_eval_retrieval(
    config: &Config,
    fixture: &str,
    k: usize,
    bootstrap: Option<usize>,
) -> Result<()> {
    use agent_brain::services::retrieval_eval;
    use std::sync::Arc;

    let client = connect_neo4j(config).await?;

    // Bootstrap needs only the graph; skip building the LLM entirely.
    if let Some(n) = bootstrap {
        let yaml = retrieval_eval::bootstrap(&client, n).await?;
        // Proposals go to stdout by design: they are meant to be redirected to a
        // file, curated (rewrite each query into a real question), and appended
        // to the fixture. The eval numbers still go through the logger.
        println!("{yaml}");
        info!(sampled = n, "Bootstrap proposals written to stdout");
        return Ok(());
    }

    let llm_config = build_llm_config(config);
    let shared = Arc::new(tokio::sync::RwLock::new(Some(llm_config)));
    let llm: Arc<dyn agent_brain::services::traits::LlmProvider> =
        agent_brain::services::shared_llm::SharedLlm::new(shared);
    let knowledge =
        agent_brain::services::knowledge::KnowledgeService::new(client.clone(), Some(llm));

    let set = retrieval_eval::load_fixture(fixture)?;
    if set.cases.is_empty() {
        warn!(
            %fixture,
            "Golden set is empty — run `eval-retrieval --bootstrap N` to seed proposals"
        );
        return Ok(());
    }

    let report = retrieval_eval::run_eval(&knowledge, &set, k).await?;
    // The report is the deliverable; print it rather than log it so the table
    // reads cleanly regardless of LOG_FORMAT.
    println!("{}", report.render());
    info!(
        k = report.k,
        recall = report.recall_at_k(),
        mrr = report.mrr(),
        "Retrieval eval complete"
    );
    Ok(())
}

async fn run_init_db(config: &Config) -> Result<()> {
    use agent_brain::services::workflow_seeder;

    let client = connect_neo4j(config).await?;
    info!("Initializing database schema...");
    client.init_schema().await?;
    info!("Database schema initialized successfully");

    // Topics first: a workflow's `topic` is checked against them, and the
    // graph's set includes topics created in the HBI. From the core's
    // and loaded plugins' topics/*.yaml; notes are classified later, by the idle-time assigner in
    // `serve`.
    let topics = agent_brain::services::topics::Topics::global();
    match agent_brain::services::topics::sync(&client, &topics).await {
        Ok(n) => info!(
            count = n,
            version = %agent_brain::services::topics::Topics::global().version,
            "Synced Topics from topics/ (HBI-edited topics kept)"
        ),
        Err(e) => warn!(error = %e, "Topic sync failed (non-fatal)"),
    }

    let plugins = agent_brain::services::plugins::Plugins::global();
    // The bundles' schemas, so the seed checks a program file as a save does.
    agent_brain::services::validation::set_envelope_schemas(std::sync::Arc::new(
        agent_brain::services::validation::load_bundle_schemas(&plugins),
    ));
    match workflow_seeder::seed_workflows(&client, &plugins).await {
        Ok(n) => info!(count = n, "Seeded workflows from the plugin bundles"),
        Err(e) => warn!(error = %e, "Failed to seed workflows (non-fatal)"),
    }
    // Programs after workflows: a program names its workflows.
    match agent_brain::services::programs::seed_programs(&client, &plugins).await {
        Ok(n) => info!(count = n, "Seeded programs from the plugin bundles"),
        Err(e) => warn!(error = %e, "Failed to seed programs (non-fatal)"),
    }

    // Entity resolution: seed aliases, keys, and the automatic merges (after a
    // snapshot). Non-fatal: the brain runs fine on unmerged entities.
    if let Err(e) = resolve_entities(&client, false).await {
        warn!(error = %e, "Entity resolution failed (non-fatal)");
    }

    Ok(())
}

async fn run_resolve_entities(config: &Config, dry_run: bool, unmerge: Option<&str>) -> Result<()> {
    let client = connect_neo4j(config).await?;
    if let Some(alias) = unmerge {
        let out = agent_brain::services::entity_resolution::unmerge(&client, alias).await?;
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    let report = resolve_entities(&client, dry_run).await?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

async fn resolve_entities(
    client: &Neo4jClient,
    dry_run: bool,
) -> Result<agent_brain::services::entity_resolution::SyncReport> {
    use agent_brain::services::entity_resolution::{Rules, sync};
    let rules = Rules::global();
    let snapshot_dir = std::env::var("KNOWLEDGE_SNAPSHOT_DIR")
        .ok()
        .map(PathBuf::from);
    let report = sync(client, rules, dry_run, snapshot_dir.as_deref()).await?;
    info!(
        dry_run,
        planned = report.planned.len(),
        merged = report.merged.len(),
        failed = report.failed.len(),
        keys_written = report.keys_written,
        snapshot = ?report.snapshot,
        "Entity resolution"
    );
    for f in &report.failed {
        warn!(merge = %f, "Entity merge failed");
    }
    Ok(report)
}

async fn run_todo(url: &str, action: TodoAction) -> Result<()> {
    let client = reqwest::Client::new();
    let base = url.trim_end_matches('/');

    match action {
        TodoAction::Add {
            title,
            description,
            priority,
            due,
            tags,
        } => {
            let tags_vec: Vec<String> = tags
                .unwrap_or_default()
                .split(',')
                .filter(|s| !s.is_empty())
                .map(|s| s.trim().to_string())
                .collect();

            let body = serde_json::json!({
                "title": title,
                "description": description,
                "priority": priority,
                "due_at": due,
                "tags": tags_vec,
            });

            let resp = client
                .post(format!("{base}/todos"))
                .json(&body)
                .send()
                .await?;

            let status = resp.status();
            let json: serde_json::Value = resp.json().await?;
            if status.is_success() {
                println!("{}", serde_json::to_string_pretty(&json)?);
            } else {
                anyhow::bail!("Server error {}: {}", status, json);
            }
        }

        TodoAction::List { status } => {
            let mut req = client.get(format!("{base}/todos"));
            if let Some(s) = status {
                req = req.query(&[("status", s)]);
            }
            let resp = req.send().await?;
            let status_code = resp.status();
            let json: serde_json::Value = resp.json().await?;
            if status_code.is_success() {
                if let Some(todos) = json.get("todos").and_then(|t| t.as_array()) {
                    if todos.is_empty() {
                        println!("No todos found.");
                    } else {
                        for todo in todos {
                            let id = todo["id"].as_str().unwrap_or("?");
                            let title = todo["title"].as_str().unwrap_or("?");
                            let status = todo["status"].as_str().unwrap_or("?");
                            let priority = todo["priority"].as_i64().unwrap_or(2);
                            let pri_label = match priority {
                                0 => "urgent",
                                1 => "high",
                                2 => "normal",
                                _ => "low",
                            };
                            println!("[{status}] [{pri_label}] {title}  (id: {id})");
                        }
                    }
                }
            } else {
                anyhow::bail!("Server error {}: {}", status_code, json);
            }
        }

        TodoAction::Done { id } => {
            let body = serde_json::json!({"status": "done"});
            let resp = client
                .put(format!("{base}/todos/{id}"))
                .json(&body)
                .send()
                .await?;
            let status_code = resp.status();
            let json: serde_json::Value = resp.json().await?;
            if status_code.is_success() {
                println!("Done: {}", json["title"].as_str().unwrap_or(&id));
            } else {
                anyhow::bail!("Server error {}: {}", status_code, json);
            }
        }

        TodoAction::Status { id, status } => {
            let body = serde_json::json!({"status": status});
            let resp = client
                .put(format!("{base}/todos/{id}"))
                .json(&body)
                .send()
                .await?;
            let status_code = resp.status();
            let json: serde_json::Value = resp.json().await?;
            if status_code.is_success() {
                println!(
                    "Updated: {} -> {}",
                    json["title"].as_str().unwrap_or(&id),
                    json["status"].as_str().unwrap_or("?")
                );
            } else {
                anyhow::bail!("Server error {}: {}", status_code, json);
            }
        }

        TodoAction::Delete { id } => {
            let resp = client.delete(format!("{base}/todos/{id}")).send().await?;
            let status_code = resp.status();
            if status_code == 204 {
                println!("Deleted: {id}");
            } else {
                let json: serde_json::Value = resp.json().await.unwrap_or_default();
                anyhow::bail!("Server error {}: {}", status_code, json);
            }
        }
    }

    Ok(())
}

// `bind`, `api_key` and `log_buffer` only feed the HTTP transport.
#[cfg_attr(not(feature = "http-transport"), allow(unused_variables))]
async fn run_serve(
    config: &Config,
    transport_type: TransportType,
    bind: &str,
    api_key: Option<String>,
    log_buffer: Arc<agent_brain::logging::LogBuffer>,
) -> Result<()> {
    let client = connect_neo4j(config).await?;

    // Configure LLM
    let llm_config = build_llm_config(config);

    // Load model catalog and determine active system prompt
    let catalog_path = PathBuf::from(&config.telemetry.model_catalog_path);
    let catalog = ModelCatalog::load_or_default(&catalog_path);
    // Before anything builds a config from the catalog: endpoints and key env
    // var names for every provider come from here.
    catalog.install_providers();
    {
        let ladder = catalog.resolve_background_fallback_ladder(&llm_config, &llm_config.model);
        info!(
            models = %ladder.iter().map(|c| c.model.as_str()).collect::<Vec<_>>().join(", "),
            "Background fallback ladder resolved"
        );
        agent_brain::services::shared_llm::install_background_fallback(ladder);
    }

    // Topic assignment: the local model classifies notes against topics/*.yaml
    // while the brain is idle (services/topics.rs). The HTTP server is the
    // deployed brain; a stdio session alongside it must not classify too.
    // Plugins' idle-time loops follow the same rule (the alias judge).
    agent_brain::services::plugins::set_background_allowed(matches!(
        transport_type,
        TransportType::Http
    ));
    if matches!(transport_type, TransportType::Http) {
        let local = Arc::new(tokio::sync::RwLock::new(Some(
            agent_brain::services::llm::build_local_llm_config(),
        )));
        agent_brain::services::topics::spawn_assigner(
            client.clone(),
            agent_brain::services::shared_llm::SharedLlm::new(local.clone()),
        );
        // Claims are stored without entity extraction; give them entities on
        // idle time (services/knowledge/entities.rs).
        agent_brain::services::knowledge::KnowledgeService::spawn_claim_entity_backfill(
            client.clone(),
            agent_brain::services::shared_llm::SharedLlm::new(local),
        );
    }

    let active_model = config.llm.model.clone();
    // Initialize Telemetry (always attempted; stub returns Err when feature disabled)
    #[allow(unused_variables)]
    let telemetry = if let Some(path) = &config.telemetry.db_path {
        // Ensure the parent directory exists (e.g. when using a named Docker volume).
        let dir_ok = if let Some(parent) = std::path::Path::new(path).parent()
            && !parent.as_os_str().is_empty()
        {
            match std::fs::create_dir_all(parent) {
                Ok(()) => true,
                Err(e) => {
                    error!(
                        path = %path,
                        directory = %parent.display(),
                        error = %e,
                        "Failed to create telemetry directory — DuckDB will not open. \
                         Disabled: QuerySkill (duckdb_query), model usage tracking."
                    );
                    false
                }
            }
        } else {
            true
        };

        if !dir_ok {
            None
        } else {
            match agent_brain::repository::TelemetryClient::new(path) {
                Ok(tc) => {
                    if let Err(e) = catalog.sync_to_duckdb(&tc) {
                        warn!("Could not sync model catalog to DuckDB: {}", e);
                    }
                    // After the catalog sync: usage rows from before the ledger
                    // recorded a provider are attributed through the catalog.
                    agent_brain::services::budget::seed_from_ledger(&tc);
                    info!("Telemetry enabled at {}", path);
                    Some(tc)
                }
                Err(e) => {
                    error!(
                        path = %path,
                        error = %e,
                        "Failed to initialize DuckDB telemetry. \
                         Disabled: QuerySkill (duckdb_query), model usage tracking."
                    );
                    None
                }
            }
        }
    } else {
        None
    };

    #[allow(unused_variables)]
    let system_prompt = catalog.resolve_system_prompt(&active_model);

    match transport_type {
        TransportType::Stdio => {
            info!("Starting MCP server on stdio...");

            let server = McpServer::new()
                .with_neo4j(client)
                .with_llm_config(llm_config);

            server.run().await?;
        }
        #[cfg(feature = "http-transport")]
        TransportType::Http => {
            // Parse bind address
            let bind_addr: SocketAddr = bind
                .parse()
                .map_err(|e| anyhow::anyhow!("Invalid bind address '{}': {}", bind, e))?;

            info!(addr = %bind_addr, "Starting MCP server on HTTP...");

            // Create shared session manager
            let session_config = agent_brain::mcp::SessionConfig::default();
            let session_manager = Arc::new(agent_brain::mcp::SessionManager::with_config(
                session_config,
            ));
            session_manager.spawn_cleanup();

            // Create thread-safe server core
            let neo4j_for_http = client.clone();
            let mut server = McpServerCore::new()
                .with_neo4j(client)
                .with_llm_config(llm_config)
                .with_session_manager(session_manager.clone())
                .with_system_prompt(system_prompt)
                .with_catalog_path(catalog_path);

            // Only create a separate chat Arc when CHAT_LLM_* overrides are
            // set. Otherwise leave chat_llm_config = None so chat_service()
            // falls back to brain.llm_config — meaning use_model() applies to
            // both the brain and the chat session immediately.
            if config.chat_llm.has_overrides() {
                server = server.with_chat_llm_config(build_chat_llm_config(config));
            }

            // Fallback ladder for chat, resolved against whichever model chat
            // actually runs — the active model is skipped inside the resolver,
            // so this has to be computed from the chat config rather than the
            // brain's.
            {
                let chat_cfg = build_chat_llm_config(config);
                let ladder = catalog.resolve_chat_fallback_ladder(&chat_cfg, &chat_cfg.model);
                if ladder.is_empty() {
                    warn!(
                        "No chat fallback ladder configured — a chat turn that produces \
                         nothing will fail rather than retry on another model"
                    );
                } else {
                    info!(
                        rungs = ladder.len(),
                        models = %ladder.iter().map(|c| c.model.as_str()).collect::<Vec<_>>().join(", "),
                        active = %chat_cfg.model,
                        "Chat fallback ladder resolved"
                    );
                }
                server = server.with_chat_fallback_ladder(ladder);
            }

            if let Some(t) = telemetry {
                server = server.with_telemetry(t);
            }

            // Configure HTTP transport
            let mut http_config = HttpTransportConfig::default()
                .with_bind_addr(bind_addr)
                .with_session_manager(session_manager)
                .with_chat_service(server.chat_service());

            // Wire Neo4j into the HTTP transport for /todos and /scheduled-tasks endpoints.
            http_config = http_config.with_neo4j_client(Arc::new(neo4j_for_http));

            // Wire scheduler handle for /scheduler-config endpoints.
            http_config = http_config.with_scheduler(server.scheduler_handle());

            // Wire context-builder, LLM config, and telemetry for the new /api/* endpoints.
            http_config = http_config.with_context_builder(server.context_builder_handle());
            http_config = http_config.with_llm_config_arc(server.llm_config_arc());
            http_config = http_config.with_chat_llm_config_arc(server.chat_llm_config_arc());
            if let Some(t) = server.telemetry() {
                http_config = http_config.with_telemetry(t);
            }

            // Wire brain event bus into the HTTP transport for SSE job notifications.
            http_config = http_config.with_brain_event_sender(server.brain.event_sender());

            // Wire the log ring buffer for GET /api/logs.
            http_config = http_config.with_log_buffer(log_buffer);

            // Wire the tool registry for GET /api/skills.
            http_config = http_config.with_tool_registry(server.tool_registry_handle());

            if let Some(key) = api_key.filter(|k| !k.is_empty()) {
                http_config = http_config.with_api_key(key);
                info!("API key authentication enabled");
            }

            let transport = HttpTransport::with_config(http_config);

            server.run_with_transport(&transport).await?;
        }
        #[cfg(not(feature = "http-transport"))]
        TransportType::Http => {
            anyhow::bail!(
                "HTTP transport is not compiled in. \
                 Rebuild with the 'http-transport' feature enabled."
            );
        }
    }

    Ok(())
}
