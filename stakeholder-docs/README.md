# Agent Brain — Stakeholder Documentation

Visual documentation for understanding how Agent Brain is built, how it works,
and how it can be applied.

## Documents

| File | Contents |
|------|----------|
| [01-overview.md](./01-overview.md) | What Agent Brain is, value propositions, system-at-a-glance diagram, skill mind map |
| [02-architecture.md](./02-architecture.md) | Layered architecture, component interaction map, Neo4j schema, LLM routing, startup sequence, context profiles |
| [03-workflows.md](./03-workflows.md) | Core workflows: memory storage/retrieval, goal execution with evaluator loop, autonomous scheduler state machine, job queue lifecycle, memory consolidation, dynamic tool creation, multi-hop reasoning |
| [04-use-cases.md](./04-use-cases.md) | Six concrete use cases with sequence/flow diagrams: research assistant, code review pipeline, team knowledge base, fully autonomous agent, procedure automation, LLM chat with persistent context |
| [05-data-flows.md](./05-data-flows.md) | End-to-end request flow, memory write/read paths, job execution flow, secret resolution, scheduler chain building |
| [06-skill-reference.md](./06-skill-reference.md) | Tool count summary, knowledge skill map, task lifecycle, job priority queue, context profile comparison, LLM provider configuration, deployment options |

## Diagram Format

All diagrams use [Mermaid](https://mermaid.js.org/) syntax and render natively in:
- GitHub Markdown
- GitLab Markdown
- Notion (with Mermaid plugin)
- VS Code (with Markdown Preview Mermaid Support extension)
- Any Mermaid-compatible renderer

## Quick Reference: Key Numbers

| Metric | Value |
|--------|-------|
| Static tools | 81+ |
| Skill domains | 15 |
| Runtime tools | N (dynamic) |
| LLM providers | 4 (Ollama, Anthropic, Gemini, OpenAI-compat) |
| Context profiles | 9 |
| Graph node types | 7 |
| Graph relationship types | 9 |
| Job priority levels | 4 |
| Test suite | 107 unit tests |
| Codebase size | ~19K LOC (Rust) |
