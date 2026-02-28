# agent-brain

The central nervous system for the agentic home lab.

## Architecture

- **Neo4j**: "The Cortex" - Knowledge Graph, Relationships, RAG, Vector Index
- **DuckDB**: "The Hippocampus" - Interaction Logs, Telemetry, Training Data
- **Axum**: HTTP/MCP Server
- **Ollama**: Local LLM Inference

## DuckDB Schema (Telemetry)

File: `brain_logs.db`

### `interactions`
| Column | Type | Description |
|--------|------|-------------|
| id | UUID | Unique ID |
| timestamp | TIMESTAMPTZ | When it happened |
| prompt | TEXT | User input |
| response | TEXT | Agent output |
| tools_used | JSON | Tool calls made |
| success | BOOLEAN | Did it work? |
| feedback_score | INTEGER | 1-5 rating (optional) |
| latency_ms | INTEGER | Duration |
| model_used | TEXT | Model name |

### `knowledge_gaps` (TODO)
Logs when the agent fails to find info or lacks a tool.
