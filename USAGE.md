# Agent Brain - Usage Guide

This agent is a local MCP server that provides API knowledge, Web Research, and RAG memory capabilities.

## Setup

1.  **Environment Variables:**
    Create a `.env` file in this directory:
    ```bash
    # Neo4j Credentials (default)
    NEO4J_URI=bolt://agent-api-neo4j-1:7687
    NEO4J_USER=neo4j
    NEO4J_PASSWORD=password

    # Search API Key (Required for 'search_web' tool)
    # Get a key from https://serpapi.com/ (Free Tier available)
    SERPAPI_KEY=your_actual_key_here
    ```

2.  **LLM Configuration:**
    Edit `docker-compose.yml` to match your local Ollama setup.
    *   **Url:** `http://host.docker.internal:11434` (to access Ollama on host)
    *   **Model:** `granite3.3:8b` (or whatever model you have pulled via `ollama pull`)

3.  **Run:**
    ```bash
    docker compose up -d --build
    ```
    *Note: Use `--build` if you have modified the Rust source code.*

## Capabilities (Skills)

### 1. Web Research (`SearchSkill`)
*   **Tool:** `search_web`
*   **Usage:** "Find documentation for DuckDB VSS"
*   **Engine:** Defaults to SerpApi (Google results).

### 2. Memory & RAG (`KnowledgeSkill`)
*   **Tool:** `store_note`
    *   Stores a text note + embeddings in Neo4j.
    *   Example: "DuckDB VSS uses HNSW indexing for vector search."
*   **Tool:** `search_notes`
    *   Retrieves notes by semantic similarity.

### 3. Task & Reflection (`TaskSkill`)
*   **Tool:** `create_task`
    *   Track high-level goals.
*   **Tool:** `reflect_on_work`
    *   **Self-Correction Loop:** Pass your `current_state` and `goal`. The agent uses the local LLM to critique the work and suggest next steps.

### 4. API Management (`ApiSkill`)
*   **Tool:** `ingest_openapi` (Learn an API from a URL)
*   **Tool:** `execute_http_request` (Call an API with self-healing)

## Testing via CURL

**Initialize Session:**
```bash
curl -X POST http://localhost:3001/mcp \
  -H "Content-Type: application/json" \
  -H "mcp-protocol-version: 2024-11-05" \
  -d '{
    "jsonrpc": "2.0", 
    "method": "initialize", 
    "id": 1, 
    "params": {
      "protocolVersion": "2024-11-05", 
      "capabilities": {}, 
      "clientInfo": {"name": "curl", "version": "1.0"}
    }
  }'
```

**Search the Web:**
```bash
# Replace SESSION_ID with the ID returned from initialize
curl -X POST http://localhost:3001/mcp \
  -H "Content-Type: application/json" \
  -H "mcp-protocol-version: 2024-11-05" \
  -H "mcp-session-id: SESSION_ID" \
  -d '{
    "jsonrpc": "2.0", 
    "method": "tools/call", 
    "id": 2, 
    "params": {
      "name": "search_web", 
      "arguments": {"query": "DuckDB VSS documentation"}
    }
  }'
```
