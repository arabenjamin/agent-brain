# Updated Content Generation

Based on the comprehensive review of the provided materials, the following document represents the finalized and updated state of the system documentation, incorporating all structural and functional changes.

***

# 🧠 System Architecture & Capabilities Overview

This document outlines the core components, operational flow, and advanced capabilities of the system, providing a comprehensive guide for developers and advanced users.

## 🚀 Core Components

The system is modular, consisting of specialized engines that interact via a central orchestration layer.

*   **Memory Core:** Manages all persistent data, including structured knowledge graphs, episodic memories, and semantic embeddings.
*   **Reasoning Engine (LLM Wrapper):** Acts as the primary inference layer, handling complex reasoning, planning, and natural language understanding (NLU).
*   **Action Executor:** Manages all external interactions, including API calls, database writes, and file system operations.
*   **Orchestrator:** Directs the flow of control, translating high-level goals into sequential, executable steps across the specialized engines.

## 🔄 Operational Flow Diagram (Conceptual)

1.  **Input:** User prompt/Goal $\rightarrow$
2.  **Orchestrator:** Decomposes Goal $\rightarrow$
3.  **Reasoning Engine:** Determines necessary steps/tools $\rightarrow$
4.  **Action Executor:** Executes tool calls/APIs $\rightarrow$
5.  **Memory Core:** Stores results, updates state $\rightarrow$
6.  **Reasoning Engine:** Synthesizes final answer $\rightarrow$
7.  **Output:** Final response to User.

## ✨ Advanced Capabilities

*   **Goal Decomposition:** Ability to break down ambiguous, high-level goals into concrete, actionable sub-tasks.
*   **Multi-Step Reasoning:** Maintains context across multiple sequential steps, allowing for complex problem-solving chains.
*   **Tool Calling:** Robust integration with external APIs (e.g., Weather, Search, Code Interpreter).
*   **Self-Correction/Reflection:** Can review its own intermediate outputs against the original goal and correct errors autonomously.

***

# 💾 Memory Management & Data Structure

The system employs a hierarchical memory structure to ensure context retention and knowledge retrieval efficiency.

## 🧠 Memory Types

1.  **Episodic Memory:** Records specific events, interactions, and temporal context (What happened, when, and to whom).
2.  **Semantic Memory:** Stores generalized facts, concepts, and relationships forming the long-term knowledge graph.
3.  **Working Memory:** Short-term context buffer used during the immediate execution of a single reasoning step.

## 🔑 Retrieval Mechanism: RAG (Retrieval-Augmented Generation)

All knowledge retrieval follows a strict RAG protocol:

1.  **Query Embedding:** The input query is converted into a high-dimensional vector.
2.  **Vector Search:** The vector is matched against the Semantic Memory index (Vector Database).
3.  **Context Retrieval:** The top-$K$ most semantically similar chunks of text/data are retrieved.
4.  **Augmentation:** These retrieved chunks are prepended to the prompt sent to the Reasoning Engine, providing grounded context.

***

# 🛠️ Tooling and Execution

The Action Executor manages the interface between the LLM's abstract plans and the real world.

## 🛠️ Available Tools (Example Set)

| Tool Name | Description | Parameters |
| :--- | :--- | :--- |
| `search_web` | Searches the live internet for current information. | `query: str` |
| `execute_code` | Runs Python code for computation or data manipulation. | `code: str` |
| `query_database` | Interacts with the structured knowledge graph (SQL/Cypher). | `query: str`, `table: str` |
| `write_file` | Saves generated content to the local file system. | `filepath: str`, `content: str` |

## ⚙️ Execution Protocol

1.  **Tool Identification:** The Reasoning Engine outputs a structured call (e.g., JSON).
2.  **Validation:** The Orchestrator validates the tool name and parameter types.
3.  **Execution:** The Action Executor runs the tool.
4.  **Observation:** The tool's raw output (`Observation`) is captured and fed back into the Reasoning Engine as context for the next turn.

***

# 📚 System Configuration & Development

This section details how to configure and extend the system.

## 🌐 Environment Variables

| Variable | Purpose | Default |
| :--- | :--- | :--- |
| `LLM_API_KEY` | Authentication key for the primary LLM provider. | *Required* |
| `VECTOR_DB_URI` | Connection string for the semantic vector store. | `sqlite:///memory.db` |
| `MAX_TOKENS` | Hard limit on the combined prompt/response size. | `4096` |

## 🚀 Extending Functionality

To add a new capability:

1.  **Define the Tool:** Create a Python function implementing the desired logic.
2.  **Wrap the Tool:** Decorate the function to make it callable by the Action Executor.
3.  **Update Schema:** Provide a detailed, natural language description of the tool and its required parameters to the Orchestrator's prompt context.

***

# ❓ FAQ & Troubleshooting

**Q: The system seems to forget what I asked three steps ago.**
**A:** This indicates a failure in context management. Check that the `Working Memory` buffer is not being prematurely cleared or that the `MAX_TOKENS` limit is not being hit.

**Q: Why does it keep using the `search_web` tool even when the answer is in the memory?**
**A:** The Reasoning Engine might be over-relying on external validation. Review the prompt to include explicit instructions: *"Only use external tools if the required information is not present in the provided context."*

**Q: How do I reset the memory?**
**A:** Run the maintenance script: `python memory_manager.py reset --full`. *Caution: This deletes all Semantic and Episodic memories.*