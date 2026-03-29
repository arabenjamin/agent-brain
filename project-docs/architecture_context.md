# Project Context: Autonomous API Knowledge Graph (Rust + MCP + Neo4j)

> **Implementation Status**: This architecture has been fully implemented. See `PLAN.md` for completion details.

## 1. High-Level Objective
Build a Model Context Protocol (MCP) server in **Rust** that acts as an "Autonomous API Expert."
The system ingests OpenAPI/Swagger specifications, maps them into a **Neo4j Graph Database**, and allows a local LLM (Ollama) to query, explore, and "test" the API via simple natural language.

**Core Value Prop:** "Don't read the docs. Ask the Knowledge Graph."
**Target User:** QA Engineers, Frontend Devs, Security Pentesters.

## 2. Tech Stack & Constraints
- **Language:** Rust (Tokio async runtime).
- **Protocol:** Model Context Protocol (MCP) - Supports both:
  - Standard Input/Output (stdio) transport for local CLI usage
  - HTTP transport with SSE streaming for remote/cloud deployment
- **Web Framework:** Axum (for HTTP transport)
- **Database:** Neo4j (GraphDB) via the `neo4rs` driver.
- **AI Model:** Local LLM (Ollama - Llama 3 or Mistral) accessed via REST.
- **Input Data:** raw JSON/YAML OpenAPI v3.0+ specs.

## 3. The Graph Schema (Neo4j Data Model)
The "Technical Moat" is how we structure the data. We do NOT store text chunks. We store the *structure* of the API.

### Nodes (Entities)
1.  **`Resource`**: A high-level grouping (e.g., "Users", "Payments").
    * `id`: string (UUID)
    * `name`: string
    * `description`: string
2.  **`Endpoint`**: A specific API path and method.
    * `path`: string (e.g., `/users/{id}`)
    * `method`: string (GET, POST, etc.)
    * `summary`: string
    * `operationId`: string
3.  **`Schema`**: A data object definition.
    * `name`: string (e.g., "UserResponse")
    * `json_structure`: string (serialized JSON of the schema)
4.  **`Parameter`**: Input required for an endpoint.
    * `name`: string
    * `in`: string (query, path, body, header)
    * `required`: boolean

### Edges (Relationships)
-   `(:Resource)-[:HAS_ENDPOINT]->(:Endpoint)`
-   `(:Endpoint)-[:REQUIRES_PARAM]->(:Parameter)`
-   `(:Endpoint)-[:RETURNS_SCHEMA {status: 200}]->(:Schema)`
-   `(:Endpoint)-[:ACCEPTS_SCHEMA]->(:Schema)`
-   `(:Schema)-[:LINKS_TO]->(:Schema)` (for nested objects)

## 4. MCP Tool Definitions (The "Arms" of the Agent)
The MCP Server exposes 14 tools to the LLM Client:

### Core Tools
-   **`ingest_openapi`** - Parse and load OpenAPI specs into Neo4j
-   **`graph_query_endpoint`** - Search endpoints by path pattern or keywords
-   **`execute_http_request`** - Execute HTTP requests with auto-credential injection

### Context Management Tools
-   **`get_api_context`** - Retrieve API summaries from in-memory context
-   **`list_loaded_apis`** - List all APIs in the context store
-   **`clear_api_context`** - Remove APIs from in-memory context

### Discovery & Generation Tools
-   **`discover_openapi`** - Auto-discover OpenAPI specs from a base URL
-   **`build_openapi_from_docs`** - Generate specs from documentation pages
-   **`build_openapi_from_repo`** - Generate specs from repository source code (GitHub/GitLab)

### Export & Diff Tools
-   **`export_openapi`** - Export healed graph back to OpenAPI 3.0 spec
-   **`diff_api_spec`** - Compare original vs healed graph, generate diff reports

### Credential Management Tools
-   **`configure_api_credential`** - Store API credentials for auto-injection
-   **`list_api_credentials`** - List all configured credentials
-   **`delete_api_credential`** - Remove an API credential

## 4.1 Secret Provider Abstraction
The system supports multiple secret storage backends:
-   **Local** - AES-256-GCM encrypted file storage (`.secrets.enc`)
-   **HashiCorp Vault** - KV v2 secrets engine with token auth
-   **AWS Secrets Manager** - AWS SDK integration with IAM auth

Credentials are automatically injected into HTTP requests based on URL matching against known API base URLs.

## 5. Critical Workflow: "Self-Healing" Documentation
1.  User asks: "Get me the user with ID 5."
2.  LLM calls `graph_query_endpoint` -> Finds `GET /users/{id}`.
3.  LLM calls `execute_http_request` -> targeting `GET /users/5`.
4.  **Scenario:** The API returns `404` or `400` because the docs were wrong (e.g., param is named `user_id`, not `id`).
5.  LLM analyzes the error, realizes the docs are stale.
6.  LLM (via internal logic) updates the `Endpoint` node in Neo4j with a `status: "Documentation Invalid"` tag.

## 6. Rust Crate Requirements

**Note**: A custom MCP server was implemented instead of using external MCP crates (which were either too immature or required nightly Rust).

-   Custom MCP implementation (`src/mcp/`) - JSON-RPC 2.0 with:
    - Stdio transport (default, for local CLI usage)
    - HTTP transport with SSE (for remote/cloud deployment)
    - Session management (HTTP only)
    - API key authentication (HTTP only)
-   `axum` + `tower-http` for HTTP transport
-   `neo4rs` for graph connection.
-   `reqwest` for the HTTP client.
-   `serde` / `serde_json` for robust typing.
-   `openapiv3` for OpenAPI spec parsing.

## 7. Logic Flow: The Self-Healing Loop
The `execute_http_request` tool must implement a retry loop with AI analysis.

**Pseudo-code logic:**
1.  **Attempt 1:** Execute request based on current Graph Schema.
2.  **Check:** If status is 200-299 -> Update Neo4j Node property `verified=true`, return result.
3.  **Branch:** If status is 4xx/5xx:
    a.  Pass the `Request`, `Error Body`, and `Graph Schema` to the LLM (internal prompt).
    b.  Ask: "Does this error suggest the schema is wrong? If so, provide the corrected JSON payload."
    c.  **Attempt 2:** Execute corrected request.
    d.  **Check:** If status is 200 -> **UPDATE NEO4J**:
        -   Modify the `Endpoint` or `Parameter` node to match reality.
        -   Add property `healed_by_ai=true`.
    e.  If status is still error -> Update Neo4j Node property `status='broken'`, return error.

## 8. Rust Data Structures: The Healing Ledger
To track changes rather than destructively overwriting data, we use a `HealingEvent` node linked to the target `Endpoint`.

### Rust Enums & Structs

```rust
use serde::{Deserialize, Serialize};
use chrono::{DateTime, Utc};
use uuid::Uuid;

/// Defines exactly what the AI changed in the graph.
/// This uses a tagged enum for precise serialization.
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "action_type", content = "details")]
pub enum HealingAction {
    /// The API doc had the wrong parameter name (e.g., 'id' -> 'user_id')
    RenameParameter {
        old_name: String,
        new_name: String,
        param_id: String, // UUID of the Parameter node
    },
    /// The API doc had the wrong data type (e.g., String -> Integer)
    ChangeParameterType {
        param_name: String,
        old_type: String,
        new_type: String,
    },
    /// The endpoint required a parameter that wasn't in the docs
    AddMissingParameter {
        param_name: String,
        required: bool,
        detected_in_error_msg: String,
    },
    /// The endpoint path itself was wrong (e.g., /v1/user -> /v2/user)
    UpdateEndpointPath {
        old_path: String,
        new_path: String,
    },
    /// The expected response schema didn't match reality
    UpdateResponseSchema {
        status_code: u16,
        diff_summary: String, // Short text description of diff
    }
}

/// The immutable record of a healing event.
/// Maps to a Neo4j Node: (:HealingEvent)
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct HealingEvent {
    pub id: Uuid,
    pub endpoint_id: String, // The UUID of the Endpoint being fixed
    pub timestamp: DateTime<Utc>,
    
    /// The specific change applied
    pub action: HealingAction,
    
    /// The raw error message from the API that triggered this fix
    /// (e.g., "400 Bad Request: 'user_id' is missing")
    pub trigger_error: String,
    
    /// The LLM's reasoning for why this fix is correct
    /// (e.g., "Error explicitly states 'user_id' is required, replacing 'id'")
    pub ai_reasoning: String,
    
    /// Was this change verified by a successful 200 OK retry?
    pub verified: bool,
}

impl HealingEvent {
    /// Constructor for a new event
    pub fn new(
        endpoint_id: String,
        action: HealingAction,
        trigger_error: String,
        reasoning: String
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            endpoint_id,
            timestamp: Utc::now(),
            action,
            trigger_error,
            ai_reasoning: reasoning,
            verified: true, // Usually only committed if verification passed
        }
    }
}```

### Cypher
// 1. Find the Endpoint
MATCH (e:Endpoint {id: $endpoint_id})

// 2. Create the Event Record
CREATE (h:HealingEvent {
    id: $event_id,
    timestamp: datetime($timestamp),
    action_type: $action_type, 
    trigger_error: $trigger_error,
    reasoning: $reasoning
})

// 3. Link Event to Endpoint (History Chain)
MERGE (e)-[:HAS_HISTORY]->(h)

// 4. Apply the mutation (Example: Renaming a Param)
// This part changes dynamically based on the HealingAction Enum
WITH e, h
MATCH (e)-[:REQUIRES_PARAM]->(p:Parameter {name: $old_param_name})
SET p.name = $new_param_name, p.last_updated = datetime()
RETURN e, h