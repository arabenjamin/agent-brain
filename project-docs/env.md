# Environment Variables

Copy `.env.example` to `.env` and configure.

## Core

| Variable | Default | Description |
|----------|---------|-------------|
| `NEO4J_URI` | `bolt://localhost:7687` | Neo4j connection URI |
| `NEO4J_USER` | `neo4j` | Neo4j username |
| `NEO4J_PASSWORD` | *required* | Neo4j password |
| `LOG_LEVEL` | `info` | `trace`/`debug`/`info`/`warn`/`error` |
| `LOG_FORMAT` | `pretty` | `pretty`/`json` |

## LLM Providers

| Variable | Default | Description |
|----------|---------|-------------|
| `LLM_PROVIDER` | `ollama` | `ollama`/`anthropic`/`gemini`/`vllm` |
| `OLLAMA_URL` | `http://localhost:11434` | Ollama API endpoint |
| `OLLAMA_MODEL` | `qwen3.5:4b` | Model for text generation |
| `OLLAMA_EMBED_MODEL` | — | Embeddings model (falls back to `OLLAMA_MODEL`) |
| `ANTHROPIC_API_KEY` | — | Anthropic API key |
| `ANTHROPIC_MODEL` | — | Anthropic model name |
| `GEMINI_API_KEY` | — | Gemini API key |
| `GEMINI_MODEL` | — | Gemini model name |
| `VLLM_URL` | `http://localhost:8000` | vLLM / OpenAI-compat server URL |
| `VLLM_MODEL` | — | Inference model (recommended: `Qwen/Qwen3-8B-AWQ` for 8 GB GPU) |
| `VLLM_API_KEY` | — | Optional Bearer token for secured deployments |
| `VLLM_EMBED_URL` | — | Separate vLLM endpoint for embeddings (e.g. `http://localhost:8001`) |
| `VLLM_EMBED_MODEL` | — | Embedding model (recommended: `BAAI/bge-m3`, 1024-dim) |

## MCP Transport

| Variable | Default | Description |
|----------|---------|-------------|
| `MCP_TRANSPORT` | `stdio` | `stdio`/`http` |
| `MCP_HTTP_BIND` | `127.0.0.1:3000` | HTTP bind address |
| `MCP_API_KEY` | — | Bearer token for HTTP auth |

## Secrets / Credentials

| Variable | Default | Description |
|----------|---------|-------------|
| `SECRET_PROVIDER` | `local` | `local`/`vault`/`aws`/`none` |
| `SECRETS_FILE` | `.secrets.enc` | Encrypted file path (local provider) |
| `SECRETS_ENCRYPTION_KEY` | — | AES-256-GCM key (required for production) |
| `VAULT_ADDR` | — | HashiCorp Vault server address |
| `VAULT_TOKEN` | — | Vault authentication token |
| `VAULT_MOUNT_PATH` | `secret` | Vault KV mount path |
| `VAULT_NAMESPACE` | — | Vault namespace (Enterprise only) |
| `AWS_REGION` | `us-east-1` | AWS region for Secrets Manager |
| `AWS_SECRET_PREFIX` | — | Prefix for AWS secret names |

## Features

| Variable | Default | Description |
|----------|---------|-------------|
| `DATASET_DIR` | `./datasets` | Training data export directory (`digest_experiences`) |
| `TELEMETRY_DB_PATH` | — | DuckDB file path (enables `SleepSkill`) |
| `SERPAPI_KEY` | — | SerpApi key for `search_web` |
| `BRAVE_API_KEY` | — | Brave Search API key |
| `GOOGLE_API_KEY` | — | Google Custom Search API key |
| `GOOGLE_CX` | — | Google Custom Search Engine ID |
| `SCHEDULER_INTERVAL_SECS` | `300` | Autonomous scheduler poll interval (seconds) |
| `SCHEDULER_ENABLED` | `true` | Set `false` to start with scheduler disabled |
| `IDLE_SLEEP_AFTER_TICKS` | `3` | Consecutive idle ticks before entering sleep mode |
| `SLEEP_INTERVAL_SECS` | `1800` | Scheduler tick interval while in sleep mode (seconds) |
| `KNOWLEDGE_SNAPSHOT_DIR` | `./snapshots` | Directory for knowledge graph snapshots |
| `AUTO_SNAPSHOT_BEFORE_CONSOLIDATION` | `true` | Snapshot before `consolidate_memories` |
| `AUTO_SNAPSHOT_BEFORE_PRUNE` | `false` | Snapshot before `prune_old_notes` |

---

## Local Development

```bash
# Start Neo4j and Ollama
docker compose up -d

# Initialize database schema
cargo run -- init-db

# Run the application
cargo run
```

## Docker Deployment (HTTP Transport)

```bash
# Build and start all services (Neo4j + MCP Server)
docker compose up -d --build

# With API key authentication
MCP_API_KEY=your-secret-key docker compose up -d --build

# View logs
docker compose logs -f agent-brain

# Health check
curl http://localhost:3000/health
```

**OpenWebUI Integration:**
- MCP URL: `http://host.docker.internal:3000/mcp` (from Docker) or `http://localhost:3000/mcp` (from host)
- Authentication: Bearer token (if `MCP_API_KEY` is set)
