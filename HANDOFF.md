# API Knowledge Graph - Product Handoff Document

> **Purpose**: Comprehensive overview for internal teams, stakeholders, and potential customers. Covers deployment options, use cases, and commercialization strategies.

---

## Executive Summary

**API Knowledge Graph** is an autonomous API documentation system that:
- Ingests OpenAPI/Swagger specs into a queryable knowledge graph
- Self-heals documentation when APIs drift from reality
- Generates OpenAPI specs from documentation pages or source code
- Integrates with AI assistants via Model Context Protocol (MCP)

**Core Value Proposition**: "Don't read the docs. Ask the Knowledge Graph."

**Target Users**: QA Engineers, Frontend Developers, DevOps, Security Teams, API Product Managers

---

## Table of Contents

1. [Deployment Methods](#deployment-methods)
2. [Modes of Use](#modes-of-use)
3. [Feature Overview](#feature-overview)
4. [Internal Use Cases](#internal-use-cases-for-startups)
5. [Commercialization Strategies](#commercialization-strategies)
6. [Technical Architecture](#technical-architecture)
7. [Getting Started](#getting-started)

---

## Deployment Methods

### 1. Local CLI (Developer Workstation)

**Best for**: Individual developers, local AI assistant integration

```bash
# Build and run locally
cargo build --release
./target/release/agent-api serve
```

**Integration with Claude CLI**:
```json
// ~/.config/claude-code/settings.json
{
  "mcpServers": {
    "api-knowledge-graph": {
      "command": "/path/to/agent-api",
      "args": ["serve"],
      "env": {
        "NEO4J_URI": "bolt://localhost:7688",
        "NEO4J_PASSWORD": "password"
      }
    }
  }
}
```

**Pros**:
- Zero latency, runs on developer machine
- No cloud costs
- Full privacy (data stays local)

**Cons**:
- Requires local Neo4j instance
- Not shared across team

---

### 2. Docker Compose (Self-Hosted)

**Best for**: Team deployments, on-premise installations, demos

```bash
# Start full stack (Neo4j + MCP Server)
docker compose up -d

# With API key authentication
MCP_API_KEY=your-secret-key docker compose up -d
```

**Endpoints**:
- `POST http://localhost:3000/mcp` - JSON-RPC requests
- `GET http://localhost:3000/mcp` - SSE stream
- `GET http://localhost:3000/health` - Health check

**Pros**:
- One-command deployment
- Shared across team
- Production-ready with health checks

**Cons**:
- Requires Docker infrastructure
- Self-managed updates

---

### 3. HTTP Transport (Cloud/Remote)

**Best for**: SaaS deployment, enterprise integration, multi-tenant scenarios

```bash
# Run with HTTP transport
cargo run -- serve --transport http --bind 0.0.0.0:8080 --api-key $SECRET

# Or via environment
MCP_TRANSPORT=http MCP_HTTP_BIND=0.0.0.0:8080 MCP_API_KEY=$SECRET ./agent-api serve
```

**Integration with OpenWebUI / Other MCP Clients**:
- MCP URL: `http://your-server:3000/mcp`
- Auth: Bearer token (API key)

**Pros**:
- Scalable, cloud-native
- Supports multiple clients
- API key authentication

**Cons**:
- Requires cloud infrastructure
- Network latency

---

### 4. Kubernetes (Enterprise)

**Best for**: Large-scale enterprise deployments

```yaml
# Example deployment (not included in repo)
apiVersion: apps/v1
kind: Deployment
metadata:
  name: api-knowledge-graph
spec:
  replicas: 3
  template:
    spec:
      containers:
      - name: mcp-server
        image: your-registry/agent-api:latest
        ports:
        - containerPort: 3000
        env:
        - name: NEO4J_URI
          valueFrom:
            secretKeyRef:
              name: neo4j-credentials
              key: uri
```

**Considerations**:
- Use managed Neo4j (Aura) or deploy Neo4j cluster
- Consider Redis for session state if scaling horizontally
- Use Kubernetes secrets for credentials

---

## Modes of Use

### Mode 1: AI Assistant Backend (MCP Server)

The primary use case - serves as a knowledge backend for AI assistants.

**How it works**:
1. AI assistant (Claude, GPT, etc.) connects via MCP protocol
2. User asks: "What endpoints handle user authentication?"
3. AI calls `graph_query_endpoint` tool
4. Knowledge graph returns matching endpoints with schemas
5. AI provides accurate, contextual answer

**Available Tools** (14 total):

| Category | Tool | Purpose |
|----------|------|---------|
| **Core** | `ingest_openapi` | Load specs into graph |
| | `graph_query_endpoint` | Search endpoints |
| | `execute_http_request` | Test APIs with auto-healing |
| **Context** | `get_api_context` | Retrieve API summaries |
| | `list_loaded_apis` | List ingested APIs |
| | `clear_api_context` | Clear cached context |
| **Discovery** | `discover_openapi` | Find specs from base URL |
| | `build_openapi_from_docs` | Generate from documentation |
| | `build_openapi_from_repo` | Generate from source code |
| **Export** | `export_openapi` | Export healed specs |
| | `diff_api_spec` | Generate drift reports |
| **Credentials** | `configure_api_credential` | Store API keys |
| | `list_api_credentials` | List credentials |
| | `delete_api_credential` | Remove credentials |

---

### Mode 2: CLI Tool (Direct Commands)

Use directly from command line without AI assistant.

```bash
# Ingest a spec
agent-api ingest https://api.example.com/openapi.json

# Query endpoints
agent-api query "payments"

# Execute and test
agent-api execute -m GET https://api.example.com/users

# Export healed spec
agent-api export -o healed-spec.yaml

# Generate diff report
agent-api diff -f markdown
```

**Use cases**:
- CI/CD pipeline integration
- Batch processing
- Scripted workflows

---

### Mode 3: HTTP API (Programmatic Access)

Integrate with any system via HTTP.

```bash
# JSON-RPC request
curl -X POST http://localhost:3000/mcp \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $API_KEY" \
  -d '{
    "jsonrpc": "2.0",
    "method": "tools/call",
    "params": {
      "name": "graph_query_endpoint",
      "arguments": {"query": "users"}
    },
    "id": 1
  }'
```

**Use cases**:
- Custom dashboards
- Integration with existing tools
- Webhooks and automation

---

## Feature Overview

### 1. OpenAPI Ingestion
- Parse JSON/YAML OpenAPI 3.0+ specs
- Extract endpoints, parameters, schemas, responses
- Build queryable knowledge graph in Neo4j

### 2. Self-Healing Documentation
When API calls fail:
1. Capture error response
2. Analyze with LLM (Ollama)
3. Suggest correction (parameter rename, type change, etc.)
4. Retry with fix
5. Update graph if successful
6. Record `HealingEvent` for audit trail

### 3. Spec Generation
- **From Documentation**: Crawl HTML/markdown docs, extract API info with LLM
- **From Source Code**: Analyze GitHub/GitLab repos, detect endpoints in any framework

### 4. Export & Diff
- Export healed graph back to OpenAPI 3.0
- Generate drift reports (original vs. current)
- Track breaking vs. non-breaking changes

### 5. Credential Management
- Store API keys securely (AES-256-GCM local, Vault, or AWS Secrets Manager)
- Auto-inject credentials based on URL matching
- Support for API key, Bearer, Basic, OAuth2

---

## Internal Use Cases (For Startups)

### 1. Accelerate Developer Onboarding

**Problem**: New developers spend days learning internal APIs.

**Solution**:
- Ingest all internal API specs
- Developers ask AI: "How do I create a new order?"
- Get instant, accurate answers with code examples

**ROI**: Reduce onboarding time from weeks to days.

---

### 2. Prevent API Documentation Drift

**Problem**: Docs get stale, causing bugs and wasted debugging time.

**Solution**:
- Run nightly: `agent-api diff --breaking-only`
- Alert on undocumented breaking changes
- Auto-generate updated specs

**ROI**: Catch drift before it causes production issues.

---

### 3. QA Automation Enhancement

**Problem**: QA writes tests against outdated API contracts.

**Solution**:
- QA asks AI: "What's the current schema for UserResponse?"
- AI returns live, verified schema from knowledge graph
- Tests always use accurate contracts

**ROI**: Reduce false test failures by 80%+.

---

### 4. Security & Compliance Auditing

**Problem**: Need to audit all API endpoints for security review.

**Solution**:
- Query: "Show all endpoints that accept user input"
- Export endpoint inventory with parameters
- Track changes over time via HealingEvents

**ROI**: Faster security audits, better compliance posture.

---

### 5. API-First Development

**Problem**: Frontend blocked waiting for backend API docs.

**Solution**:
- Generate spec from backend repo before API is deployed
- Frontend develops against generated spec
- Iterate in parallel

**ROI**: Faster feature delivery, less cross-team blocking.

---

## Commercialization Strategies

### Strategy 1: SaaS Product (API Documentation Platform)

**Model**: Monthly subscription per workspace

**Tiers**:
| Tier | Price | Features |
|------|-------|----------|
| Free | $0 | 1 API, 100 endpoints, community support |
| Team | $99/mo | 10 APIs, unlimited endpoints, SSO |
| Enterprise | Custom | Unlimited, SLA, dedicated support, on-prem option |

**Differentiation**:
- Self-healing (competitors don't have this)
- AI-native (built for LLM integration)
- Multi-source generation (docs, code, discovery)

**Go-to-Market**:
1. Launch on Product Hunt
2. Content marketing (blog posts on API doc pain points)
3. Integration partnerships (Postman, Swagger, ReadMe)

---

### Strategy 2: Enterprise Add-On (API Governance)

**Model**: Sell to enterprises with large API portfolios

**Value Prop**: "API Governance Autopilot"
- Automatic drift detection across 100s of APIs
- Compliance reporting (SOC2, HIPAA audit trails)
- Self-healing reduces support tickets

**Pricing**: $50k-500k/year based on API count

**Sales Motion**:
1. Target API Platform teams at F500
2. POC with 5-10 APIs
3. Expand to full portfolio

---

### Strategy 3: Developer Tool (Open Core)

**Model**: Open source core, paid cloud/enterprise features

**Open Source** (current repo):
- All 14 tools
- Local deployment
- Community support

**Paid Features**:
- Hosted cloud version
- Team collaboration
- Advanced analytics dashboard
- Priority support
- Custom integrations

**Go-to-Market**:
1. Build community around open source
2. Offer cloud version for convenience
3. Enterprise features for large teams

---

### Strategy 4: Embedded/OEM Licensing

**Model**: License to other platforms

**Targets**:
- API management platforms (Kong, Apigee)
- Documentation tools (ReadMe, Stoplight)
- AI coding assistants (Cursor, Copilot alternatives)

**Value Prop**: "Add self-healing API intelligence to your platform"

**Pricing**: Revenue share or per-seat licensing

---

### Strategy 5: Professional Services

**Model**: Consulting + implementation

**Services**:
- API documentation audit ($5-20k)
- Knowledge graph setup & training ($10-50k)
- Custom integration development ($20-100k)
- Ongoing managed service ($5-20k/mo)

**Best For**: Enterprises with complex, legacy API landscapes

---

## Technical Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                    Client Layer                             │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐      │
│  │ Claude CLI   │  │   HTTP API   │  │  Direct CLI  │      │
│  │ (MCP stdio)  │  │  (REST/SSE)  │  │  (commands)  │      │
│  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘      │
└─────────┼─────────────────┼─────────────────┼───────────────┘
          │                 │                 │
┌─────────▼─────────────────▼─────────────────▼───────────────┐
│                    MCP Server (Rust)                        │
│  ┌─────────────────────────────────────────────────────┐   │
│  │              Transport Layer                         │   │
│  │  ┌─────────────┐          ┌─────────────────────┐   │   │
│  │  │   Stdio     │          │   HTTP (Axum+SSE)   │   │   │
│  │  └─────────────┘          └─────────────────────┘   │   │
│  └─────────────────────────────────────────────────────┘   │
│  ┌─────────────────────────────────────────────────────┐   │
│  │              Tool Layer (14 tools)                   │   │
│  └─────────────────────────────────────────────────────┘   │
│  ┌─────────────────────────────────────────────────────┐   │
│  │              Service Layer                           │   │
│  │  ┌────────┐ ┌────────┐ ┌────────┐ ┌─────────────┐   │   │
│  │  │OpenAPI │ │  HTTP  │ │  LLM   │ │  Healing    │   │   │
│  │  │Parser  │ │Executor│ │Client  │ │Orchestrator │   │   │
│  │  └────────┘ └────────┘ └────────┘ └─────────────┘   │   │
│  │  ┌────────┐ ┌────────┐ ┌────────┐ ┌─────────────┐   │   │
│  │  │Context │ │Discover│ │DocGen  │ │ RepoAnalyze │   │   │
│  │  │ Store  │ │Service │ │Service │ │  Service    │   │   │
│  │  └────────┘ └────────┘ └────────┘ └─────────────┘   │   │
│  │  ┌──────────────────────────────────────────────┐   │   │
│  │  │           Secrets Module                      │   │   │
│  │  │  Local (AES) │ Vault │ AWS Secrets Manager   │   │   │
│  │  └──────────────────────────────────────────────┘   │   │
│  └─────────────────────────────────────────────────────┘   │
└─────────────────────────┬───────────────────────────────────┘
                          │
┌─────────────────────────▼───────────────────────────────────┐
│                    Data Layer                               │
│  ┌─────────────────────────────────────────────────────┐   │
│  │                 Neo4j Knowledge Graph                │   │
│  │                                                      │   │
│  │  (Resource)──[:HAS_ENDPOINT]──▶(Endpoint)           │   │
│  │                                      │               │   │
│  │                     ┌────────────────┼────────────┐  │   │
│  │                     ▼                ▼            ▼  │   │
│  │               (Parameter)       (Schema)  (HealingEvent)│
│  │                                                      │   │
│  │  (ApiCredential) ← Credential metadata               │   │
│  └─────────────────────────────────────────────────────┘   │
│  ┌─────────────────────────────────────────────────────┐   │
│  │                    Ollama (LLM)                      │   │
│  │              Llama 3 / Mistral / etc.               │   │
│  └─────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────┘
```

---

## Getting Started

### Quick Start (Local)

```bash
# 1. Clone and build
git clone <repo-url> && cd agent-api
cargo build --release

# 2. Start dependencies
docker compose up -d neo4j

# 3. Initialize database
./target/release/agent-api init-db

# 4. Ingest your first API
./target/release/agent-api ingest https://petstore3.swagger.io/api/v3/openapi.json

# 5. Query it
./target/release/agent-api query "pets"
```

### Quick Start (Docker)

```bash
# One command to run everything
docker compose up -d

# Test the health endpoint
curl http://localhost:3000/health
```

### Connect to Claude CLI

```bash
# Copy the example config
cp .mcp.json.example ~/.config/claude-code/mcp.json

# Edit paths and credentials
vim ~/.config/claude-code/mcp.json

# Restart Claude CLI
```

---

## Support & Resources

- **Documentation**: `CLAUDE.md`, `README.md`, `PLAN.md`
- **Architecture**: `architecture_context.md`
- **QA Overview**: `docs/ONE-PAGER-QA.md`
- **Issues**: GitHub Issues
- **Tests**: `cargo test` (226 unit tests, 30+ integration tests)

---

## Appendix: Competitive Landscape

| Competitor | Self-Healing | AI-Native | Code Analysis | Open Source |
|------------|--------------|-----------|---------------|-------------|
| **API Knowledge Graph** | ✅ | ✅ | ✅ | ✅ |
| Swagger/OpenAPI Tools | ❌ | ❌ | ❌ | ✅ |
| Postman | ❌ | Partial | ❌ | ❌ |
| ReadMe | ❌ | Partial | ❌ | ❌ |
| Stoplight | ❌ | ❌ | ❌ | ❌ |

**Key Differentiators**:
1. **Self-healing** - No competitor auto-fixes documentation drift
2. **AI-native** - Built for MCP/LLM integration from day one
3. **Multi-source generation** - Docs, code, discovery in one tool
4. **Open source** - Full transparency, community contributions

---

*Document generated for internal handoff and stakeholder communication.*
