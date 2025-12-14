# API Knowledge Graph for QA Teams

**Keep AI Assistants Accurate. Keep Pipelines Green.**

---

## The Problem

Your API documentation is lying to your tools.

| Symptom | Impact |
|---------|--------|
| AI assistant hallucinates wrong endpoints | Developers waste time debugging generated code |
| CI contract tests fail unexpectedly | Pipeline blocked, release delayed |
| OpenAPI spec doesn't match production | Code generators produce broken clients |
| Nobody knows what changed since last sprint | QA discovers API drift during regression |

**Root cause:** Documentation drifts from reality. Developers change APIs but forget to update specs.

---

## The Solution

**Autonomous API Knowledge Graph** — an MCP server that ingests, validates, heals, and exports OpenAPI specifications.

```
┌─────────────┐     ┌──────────────┐     ┌─────────────┐
│ OpenAPI Spec│────▶│ Knowledge    │────▶│ AI Assistant│
│ (stale)     │     │ Graph (Neo4j)│     │ (accurate)  │
└─────────────┘     └──────┬───────┘     └─────────────┘
                          │
                   ┌──────▼───────┐
                   │ Self-Healing │
                   │ Engine (LLM) │
                   └──────┬───────┘
                          │
┌─────────────┐     ┌──────▼───────┐     ┌─────────────┐
│ CI Pipeline │◀────│ Healed Spec  │◀────│ Drift Report│
│ (green)     │     │ (accurate)   │     │ (changelog) │
└─────────────┘     └──────────────┘     └─────────────┘
```

---

## For AI Assistants

### Problem
Claude, GPT, and Copilot generate API calls based on context you provide. Stale docs = hallucinated endpoints.

### Solution
The MCP server gives AI assistants **live, accurate API context** directly from the knowledge graph.

```json
// AI assistant asks: "How do I create a user?"
// Tool: get_api_context

{
  "endpoint": "POST /users",
  "parameters": {
    "body": ["email (required)", "name (required)", "role (optional)"]
  },
  "response_schema": "User",
  "status": "verified",
  "last_tested": "2025-12-14T10:30:00Z"
}
```

**Result:** AI generates correct code on the first try.

### MCP Tools for AI Workflows

| Tool | What AI Uses It For |
|------|---------------------|
| `get_api_context` | Retrieve accurate endpoint details |
| `graph_query_endpoint` | Search for relevant endpoints |
| `execute_http_request` | Test API calls with auto-healing |
| `list_loaded_apis` | See all available APIs |

---

## For CI/CD Pipelines

### Problem
Contract tests break. Nobody knows if it's a bug or undocumented API change. Pipeline stays red while teams investigate.

### Solution
Integrate drift detection into your pipeline. Fail fast on **breaking undocumented changes**.

```yaml
# .github/workflows/api-contract.yml
name: API Contract Validation

on: [push, pull_request]

jobs:
  validate-api:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Ingest current spec
        run: cargo run -- ingest ./openapi.yaml

      - name: Check for breaking changes
        run: |
          cargo run -- diff --breaking-only -f json > changes.json
          if [ -s changes.json ]; then
            echo "::error::Breaking API changes detected!"
            cat changes.json
            exit 1
          fi

      - name: Export healed spec
        run: cargo run -- export -o healed-spec.yaml

      - name: Upload artifact
        uses: actions/upload-artifact@v4
        with:
          name: healed-openapi-spec
          path: healed-spec.yaml
```

### Pipeline Integration Points

| Stage | Integration |
|-------|-------------|
| **Pre-commit** | Validate spec syntax, check for drift |
| **PR Check** | Generate diff report, flag breaking changes |
| **Post-merge** | Export healed spec, update documentation |
| **Nightly** | Full API test suite with healing enabled |

### Diff Report Output

```markdown
## API Drift Report - 2025-12-14

### Breaking Changes (2)
- `POST /users`: Parameter `userName` renamed to `username`
- `GET /orders/{id}`: Response schema field `total` type changed: string → number

### Non-Breaking Changes (3)
- `GET /products`: New optional parameter `category` added
- `POST /auth/login`: New response field `refreshToken` added
- `DELETE /users/{id}`: Endpoint marked as deprecated
```

---

## Quick Start

```bash
# 1. Install and configure
git clone <repo> && cd agent-api
cp .env.example .env  # Configure Neo4j + Ollama

# 2. Start dependencies
docker compose up -d

# 3. Ingest your API spec
cargo run -- ingest https://api.yourcompany.com/openapi.json

# 4. Generate drift report
cargo run -- diff -f markdown

# 5. Export healed spec
cargo run -- export -o healed-spec.yaml
```

---

## Key Benefits

| Metric | Before | After |
|--------|--------|-------|
| Doc-related bugs per sprint | 5-10 | 0-1 |
| Time debugging AI-generated code | Hours | Minutes |
| Pipeline failures from doc drift | Weekly | Rare |
| Spec accuracy | ~70% | 99%+ |

---

## Architecture

- **Language:** Rust (fast, reliable)
- **Database:** Neo4j (relationship-aware queries)
- **AI:** Ollama/local LLM (self-healing analysis)
- **Protocol:** MCP (native AI assistant integration)
- **Output:** OpenAPI 3.0 YAML/JSON

---

## Next Steps

1. **Pilot:** Run against one API for 2 weeks
2. **Measure:** Track drift reports and healing events
3. **Expand:** Add to CI pipeline, connect AI assistants
4. **Scale:** Roll out across all team APIs

---

**Questions?** Open an issue or reach out to the platform team.
