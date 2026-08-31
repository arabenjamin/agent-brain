# Brain-to-Brain Transport Plan — Federated Knowledge Exchange Between Brain Instances

Handoff plan for letting multiple Agent Brain instances exchange knowledge with
**provenance that survives storage**. Goal: brain A can assert a claim to brain B
such that brain B can still verify *who said it* months later, after the message
has been relayed, stored, consolidated, and retrieved.

This document is the spec. Implement it in phases; each phase is independently
shippable and testable.

---

## 0. The reframe that makes this tractable

The obvious framing — "pick a mesh network for the brains" — is the wrong
question, and answering it first produces an unbuildable design. (See
`CLAUDE.md` § *Self-knowledge*: a chat session on 2026-08-23 confidently
proposed a three-tier Nebula/Meshtastic/Reticulum architecture that could not
work, because Nebula is an IP overlay, Meshtastic carries ~1 kbps of non-IP
packets, and Reticulum has no IP layer at all.)

Two observations reorder the problem:

1. **Brain-to-brain traffic is message-shaped, not RPC-shaped.** What brains send
   each other is claims, notes, source records, task offers, and capability
   announcements. Small, self-contained, latency-tolerant, safe to reorder. It is
   not "call this function and block."
2. **Every transport gives transport authentication, and transport
   authentication is not provenance.** TLS, WireGuard, mTLS, QUIC — all of them
   authenticate *the channel*. The instant a payload leaves the connection and
   lands in Neo4j, that guarantee is gone and the brain is trusting its own
   ingest path. But `asserted_by` is a durable graph property that outlives the
   connection by months.

So the load-bearing artifact is **a signed envelope**, not a network. The
transport is a swappable detail — and once the envelope exists, we can run more
than one transport with different failure modes.

---

## 1. Design decisions (locked)

| Decision | Choice |
|----------|--------|
| Primary abstraction | **Signed envelope, transport-agnostic.** Defined once as a `.proto`; every transport carries the same bytes. |
| Identity | **One Ed25519 keypair per brain instance**, independent of transport. The public key *is* the brain's address on every transport. |
| Trust model | **Message-level signatures, verified at ingest.** Transport auth is defence-in-depth, never the provenance record. |
| Transport count | **Two, with different failure modes.** Transport A (online, high-throughput) and Transport B (degraded, permissionless, store-and-forward). Never one. |
| Transport A, phase 1 | **HTTP + protobuf over Tailscale.** Already deployed, already trusted, zero new infrastructure. |
| Transport B | **LXMF over Reticulum**, via a Python sidecar. Survives no-IP and offline peers. |
| Reticulum implementation | **RetiNet (AGPL-3.0) or Reticulum_CE — never `markqvist/Reticulum`.** See § 6.1; the upstream license conflicts with `digest_experiences`. |
| Peer list | **`peers/*.yaml`, seeded ON CREATE, graph-owned afterwards.** Follows the `SourceList` / `MediaSource` pattern exactly. |
| Peer claims and independence | **A peer brain never counts toward `MIN_INDEPENDENT_DOMAINS`.** New `peer_brain` tier in `classify_domains`. See § 5.2 — this is the highest-risk item in the plan. |
| Rejected | WebRTC, Meshtastic, IP-over-Reticulum, bare Tailscale-only. See § 7. |

---

## 2. Why this fits the current architecture

Most of the primitives already exist:

- **The claims layer is the consumer.** `(:Note {note_type:'claim', claim_status,
  asserted_by, asserted_at})` with `ASSERTED_IN` / `CORROBORATED_BY` /
  `CONTRADICTED_BY` edges, status *derived* by `recompute_status` rather than
  asserted. A peer brain's identity hash is a far better `asserted_by` value
  than a domain string, because it is self-certifying.
- **Retrieval labelling already exists.** `label_claims` in
  `services/knowledge.rs` prefixes claims and source records on the way out.
  Peer-sourced material needs one more prefix variant, not a new mechanism.
- **The sidecar pattern is established.** `sandbox`, `whisper`, and `searxng` are
  all compose services the brain talks to over HTTP on an internal network. A
  Reticulum sidecar is the fourth instance of a pattern, not a new one.
- **YAML-seeded, graph-owned config lists.** `sources/*.yaml` and
  `sources-media/*.yaml` are seeded ON CREATE and owned by the graph afterwards.
  `peers/*.yaml` is the same seeder with a different node label.
- **Timestamps are already one representation.** Every temporal property is a
  native `DATETIME` in UTC (`project-docs/schema.md`), so a received envelope's
  timestamps have an unambiguous home. Envelope wire format stays RFC 3339
  strings; conversion happens at ingest via `datetime($param)`.
- **Snapshots already exist for bulk sync.** `services/snapshot.rs` produces
  gzipped JSON. Transport A's bulk path is "ship a snapshot", not a new
  serialization format.

**Net new surface:** one crate-level module for identity/envelope, one skill, one
sidecar, one seed dir, and three graph additions.

---

## 3. Architecture

```
                    ┌──────────────────────────────────┐
                    │  Layer 3 — Ingest & epistemics   │
                    │  verify sig → classify → graph   │
                    └────────────────┬─────────────────┘
                                     │
                    ┌────────────────▼─────────────────┐
                    │  Layer 2 — Envelope (.proto)      │
                    │  signed, transport-agnostic       │
                    └────┬──────────────────────┬───────┘
                         │                      │
              ┌──────────▼─────────┐  ┌─────────▼──────────┐
              │   Transport A      │  │   Transport B      │
              │  online / bulk     │  │  degraded / async  │
              │  HTTP over         │  │  LXMF over         │
              │  Tailscale         │  │  Reticulum         │
              │  → iroh (phase 5)  │  │  (Python sidecar)  │
              └──────────┬─────────┘  └─────────┬──────────┘
                         │                      │
                    ┌────▼──────────────────────▼───────┐
                    │  Layer 0 — Identity                │
                    │  one Ed25519 keypair per brain     │
                    └────────────────────────────────────┘
```

The two transports are **not** a primary and a fallback in the failover sense.
They carry different traffic:

| | Transport A | Transport B |
|---|---|---|
| Carries | snapshots, embeddings, transcripts, bulk note sync | claims, task offers, capability announces |
| Payload size | MB | < 1 KB |
| Peer must be online | yes | no (propagation nodes hold messages) |
| Needs IP | yes | no |
| Needs a control plane | yes (tailnet) | no |

---

## 4. New components

### 4.1 `crates/models/src/envelope.rs` + `proto/brain_envelope.proto`

The envelope is the durable design decision, so it is defined first and
separately from any transport.

```proto
syntax = "proto3";
package brain.v1;

message Envelope {
  uint32 version          = 1;  // wire format version, starts at 1
  bytes  sender_pubkey    = 2;  // 32-byte Ed25519 public key
  string sender_name      = 3;  // human label, NOT trusted, display only
  string message_id       = 4;  // UUID, for idempotent ingest
  string created_at       = 5;  // RFC 3339 UTC
  Payload payload         = 6;
  bytes  signature        = 7;  // Ed25519 over canonical bytes of 1-6
}

message Payload {
  oneof kind {
    ClaimAssertion      claim       = 1;
    NoteShare           note        = 2;
    SourceRecordShare   source      = 3;
    TaskOffer           task_offer  = 4;
    CapabilityAnnounce  capability  = 5;
  }
}
```

Rules that make it safe:

- **`sender_name` is never trusted.** It is display-only. Identity is the
  pubkey, always. A peer that renames itself is still the same peer.
- **The signature covers fields 1–6 in a canonical encoding.** Protobuf is not
  canonical by default (field ordering, unknown fields), so sign a
  deterministically re-serialized form — sort fields, reject unknown fields on
  the verify path — or sign a domain-separated hash of explicitly concatenated
  fields. Pick one and write a round-trip test; this is the single easiest place
  to build a subtly forgeable system.
- **`message_id` makes ingest idempotent.** Transport B can legitimately deliver
  the same message twice via two propagation nodes.
- **`version` gates everything.** An envelope with an unknown version is
  rejected and logged at WARN, never best-effort parsed.

### 4.2 `crates/app/src/services/peer_identity.rs`

Loads or generates the brain's keypair. Storage: `SECRET_PROVIDER` (the existing
local AES-GCM / Vault / AWS abstraction), **not** a file next to the binary —
this key is the brain's identity and leaking it lets anyone forge its assertions.

Exposes `sign(&Envelope) -> Signature` and `verify(&Envelope) -> Result<PeerId>`.

### 4.3 `crates/app/src/skills/peer.rs` — new skill

| Tool | Purpose |
|------|---------|
| `peer_send` | Assert a claim / share a note / offer a task to a named peer or broadcast |
| `peer_list` | List known peers, their transports, last-seen, trust state |
| `manage_peer` | Upsert / activate / deactivate a peer (mirrors `manage_media_source`) |

Register to **both** `tool_registry` and the `skills` vec in `build_skills()` —
forgetting either yields an invisible tool or a dispatch failure.

`peer_send` must be **safe to insert mid-chain**, which means it echoes its input
as `answer` (like `store_note`, `notify_user`, and `claim` do). A tool that
silently replaces the chain payload with its own metadata breaks every downstream
step — see `CLAUDE.md` on the `claim` tool echo fix.

### 4.4 `reticulum-sidecar/` — new compose service

Python, owns the RNS Identity and LXMF. Exposes a minimal HTTP/JSON API on
`brain-internal`:

- `POST /send` — `{peer_hash, envelope_b64}` → queued for LXMF delivery
- `GET  /inbox` — long-poll or drain received envelopes
- `GET  /status` — RNS interface state, propagation node reachability

Compose constraints, modelled on the `sandbox` service:

- On `brain-internal` only, **no `ports:` mapping**.
- Needs *outbound* network for its Reticulum interfaces (unlike `sandbox`, which
  is deliberately on an `internal: true` network). This is the one sidecar that
  legitimately reaches the outside world, so it gets no credentials: no
  `env_file`, therefore no `NEO4J_PASSWORD` / `OLLAMA_API_KEY` / `GITHUB_TOKEN`.
- `cap_drop: ALL`, `no-new-privileges`, `read_only: true` root with a named
  volume for the RNS identity + config.

### 4.5 `peers/*.yaml` — new seed dir

```yaml
name: workshop-brain
pubkey: "b3f1…"            # 32-byte Ed25519, hex
description: "Bench instance, LoRa-attached"
active: true
transports:
  - kind: tailscale_http
    endpoint: "http://workshop-brain:3000"
  - kind: reticulum_lxmf
    destination_hash: "a17c…"
trust:
  accept_claims: true
  accept_task_offers: false   # default deny — see § 5.3
```

Seeded ON CREATE only, graph-owned afterwards (the `SourceList` model). Missing
directory is non-fatal. **Add `PEERS_DIR` to compose env** — a new seed dir that
isn't in the compose environment is invisible in Docker, which is where the brain
actually runs.

### 4.6 Graph additions

```
(:PeerBrain {pubkey, name, first_seen_at, last_seen_at, active, managed_by})
(:Note)-[:RECEIVED_FROM {message_id, received_at, transport}]->(:PeerBrain)
(:PeerBrain)-[:ASSERTED]->(:Note {note_type:'claim'})
```

`pubkey` is the unique key, not `name`. All three timestamps are native
`DATETIME` — the `no_string_timestamps` integration test will catch it if not.

---

## 5. Epistemics integration — the part that matters most

Getting bytes between brains is the easy half. This is the half that decides
whether federation makes the brain smarter or just louder.

### 5.1 Received claims are labelled, not laundered

A claim arriving from a peer is **not** knowledge this brain established. It
takes `note_type: 'claim'` with `asserted_by` set to the peer's pubkey, and
`label_claims` gains a variant:

```
[CLAIM · unverified · asserted by peer workshop-brain (b3f1…) · 2 days ago]
```

The peer name appears for readability; the truncated pubkey appears because names
are untrusted. Both, always — a label with only the name is a label that can lie.

### 5.2 A peer brain is never independent corroboration

**This is the highest-risk item in the plan.** `check_independence` currently
requires `MIN_INDEPENDENT_DOMAINS` (2) distinct non-self-referential domains
before support counts. Naively feeding peer claims into that check breaks it in
the exact way the existing code already tries to prevent:

- Identity keypairs are **free to generate**. N brain identities are not N
  independent sources.
- A fleet of *your own* brains corroborating each other is precisely the circular
  case `check_independence` rejects for `skywatcher.ai`.
- Worse, the brains share ingest paths, source lists, and models — so they will
  frequently agree because they read the same page, not because two independent
  observers converged.

Required changes:

- Add a **`peer_brain` tier** to `classify_domains`, alongside `primary`,
  `established`, and `unclassified`.
- Peer assertions **never increment the independent-domain count.** They are
  recorded, labelled, and visible — but a claim corroborated only by peer brains
  stays `unverified`.
- Label renders the distinction explicitly:
  `[CLAIM · corroborated · peer brains only, not independent · …]`
- A peer claim's *underlying* sources (the URLs it cites) **do** count normally —
  those are real independent domains. Propagate them in `ClaimAssertion` so
  corroboration works on evidence rather than on hearsay.

The general rule, consistent with the rest of the epistemics layer: never gate
*storage* on trust tier, always gate *status* on it. Dropping peer claims would
destroy the record needed to notice a peer drifting; promoting them would launder
an assertion into established knowledge.

### 5.3 Task offers default to deny

`TaskOffer` lets a peer enqueue work in this brain. That is remote code execution
by a slightly longer path — the scheduler will route the goal to a chain and run
it. So:

- `accept_task_offers: false` is the **default** in `peers/*.yaml`.
- An accepted offer creates a `(:Task)` with `status: 'created'` and the peer's
  pubkey in `context`, so chain-death attribution and the evaluator loop both
  work normally.
- Offers are rate-limited per peer. A peer that can create unbounded tasks can
  exhaust the queue and the LLM quota.

### 5.4 Contradiction between brains is a first-class outcome

Two brains asserting incompatible claims should produce `disputed`, not a winner.
`recompute_status` already handles support-and-contradiction without collapsing
to a verdict, so this mostly falls out — but the reconciliation path must not
"prefer local" or "prefer newest". Both assertions are preserved with their
provenance.

---

## 6. Constraints discovered during research

### 6.1 The Reticulum license conflicts with `digest_experiences`

Reticulum License 1.0 states:

> The Software shall not be used, directly or indirectly, in the creation of an
> artificial intelligence, machine learning or language model training dataset,
> including but not limited to any use that contributes to the training or
> development of such a model or algorithm.

The brain's `digest_experiences` / `SleepSkill` exports training data to
`DATASET_DIR`. That is literally creating a training dataset. The author's own
license primer argues "use" means technical incorporation rather than content
flowing through, and explicitly permits open-source AI development — but that
reading should not be load-bearing for a system whose stated purpose includes
training-data export.

**Mitigation, and it is clean:** the Reticulum *protocol* is public domain. Use a
wire-compatible implementation under an ordinary license:

| Implementation | License | Notes |
|---|---|---|
| RetiNet (`codeberg.org/skyguy/retinet`) | AGPL-3.0 | Drop-in RNS replacement, RNS 1.0 compatible. **Verify PyPI packaging** — Codeberg-hosted. |
| Reticulum_CE (`Reticulum-Community/Reticulum_CE`) | community fork | Same compatibility claim |
| Reticulum-rs (`BeechatNetworkSystemsLtd`) | MIT | Rust, but **LXMF not documented as supported** — see § 6.3 |

Running Reticulum in a *sidecar* rather than linking it into the Rust binary also
keeps the question at arm's length regardless of which implementation is chosen.

### 6.2 Reticulum's governance is unsettled

Mark Qvist announced he was leaving development in December 2025; the issue
tracker was hidden and community management stopped. The ecosystem has since
fragmented across the Python original, RetiNet, Reticulum_CE, Reticulum-rs,
microReticulum (embedded C++), and a Go+WASM implementation — with known config
incompatibility between them. FOSDEM 2026 filled two sessions working out what to
do.

The protocol is stable and public domain; the *ecosystem* is in flux. This is a
direct argument for the two-transport design: Transport B must never be the only
way two brains can talk.

### 6.3 Reticulum's Rust implementation lacks the piece we want

Reticulum-rs (MIT, actively developed) supports TCP, serial, and Kaonic — but
LXMF is not documented as supported, and neither is AutoInterface or RNode/LoRa.
LXMF store-and-forward is the entire reason Transport B exists, so the Rust path
does not currently work. Hence the Python sidecar. Revisit if `rsLXMF` matures.

### 6.4 Reticulum's byte budget shapes what can cross it

Packet data field is 0–465 bytes; it needs only a 500-byte physical MTU and works
down to ~5 bits/sec. Large transfers use the `Resource` mechanism
(auto-compress, chunk, sequence, verify, reassemble) over an established Link,
but slowly. This is why snapshots go over Transport A and claims go over
Transport B — and why Phase 0 measures before anything is built.

---

## 7. Rejected alternatives

| Option | Why not |
|---|---|
| **WebRTC** | Its value proposition is NAT traversal via ICE/STUN/TURN. Tailscale already solves exactly that. Adopting it means reimplementing the hard part of Tailscale inside the application, building an SDP signaling channel, and operating a TURN server, to obtain a capability already deployed. Revisit only if a browser must be a *peer* rather than a client — and then prefer WebTransport. |
| **gRPC as the transport** | The *contract* half is valuable and is adopted (protobuf envelope, § 4.1). The transport half assumes a reachable `host:port` with no NAT traversal and no store-and-forward, so it cannot be Transport B. Adding `tonic` also means a second RPC paradigm and `protoc` in the build, alongside the HTTP+JSON the brain already speaks everywhere (MCP, sandbox, whisper, searxng). Not worth the tax at low message volume. |
| **Meshtastic** | Wrong layer. No general transport abstraction, flood routing that degrades badly, ~200-byte payloads, built for human chat and GPS. Note RNode and Meshtastic are alternative firmwares for overlapping LoRa boards, so hardware spend isn't wasted either way. |
| **IP-over-Reticulum** | Does not exist. There is no TUN device to point Bolt or HTTP at. `rngit` exists for git specifically and the manual warns it "has not been tested extensively in the wild." |
| **Tailscale/Nebula alone** | Gives transport auth only. Provenance dies at the graph boundary, which defeats the purpose (§ 0). Fine as *a* transport — it is Transport A — but not as the whole design. |
| **Nebula instead of Tailscale** | Not rejected, just orthogonal. Transport A talks HTTP to an endpoint; whether that endpoint is reachable via Tailscale or Nebula is a deployment detail the envelope never sees. Migrating later costs nothing. |

---

## 8. Phases

Each phase is independently shippable and leaves the system working.

### Phase 0 — Measure before building

No code. Answer these with numbers:

- How large is a real knowledge snapshot (`services/snapshot.rs` output, gzipped)?
- How many claims per day does the brain currently extract? (That is Transport B's
  actual load.)
- Bring up two RetiNet nodes on the LAN — `AutoInterface` discovers them via IPv6
  link-local multicast with zero config — and time `rncp` of a real snapshot.
  Inspect with `rnstatus` / `rnpath` / `rnprobe`.

**Exit criterion:** a measured split of which traffic belongs on which transport.
The architecture diagram in § 3 asserts this split; Phase 0 verifies it. If a
snapshot crosses Reticulum acceptably, Transport A gets simpler. If claims are
higher-volume than expected, Transport B needs batching.

### Phase 1 — Identity and envelope

`proto/brain_envelope.proto`, `crates/models/src/envelope.rs`,
`services/peer_identity.rs`. Key stored via `SECRET_PROVIDER`.

**Tests:** sign/verify round-trip; canonical-encoding stability across
re-serialization; tampered-field rejection; unknown-version rejection; unknown
pubkey rejection.

**Exit criterion:** a brain can sign an envelope and verify its own, with no
network involved.

### Phase 2 — Transport A over Tailscale

`peers/*.yaml` + seeder, `(:PeerBrain)` node, `skills/peer.rs` with `peer_send` /
`peer_list` / `manage_peer`. HTTP POST of a protobuf envelope to a peer's
existing HTTP transport, plus a receive endpoint.

**Exit criterion:** two brains on the tailnet exchange a signed `NoteShare`,
verified at ingest, with a `RECEIVED_FROM` edge in the graph.

### Phase 3 — Ingest and epistemics

The `peer_brain` tier in `classify_domains`, the `label_claims` variants, the
independence exclusion (§ 5.2), task-offer default-deny and rate limiting
(§ 5.3), idempotent ingest on `message_id`.

**Exit criterion:** a claim asserted by a peer and corroborated *only* by peers
remains `unverified`, and retrieval labels it as peer-sourced. This is the test
that proves federation didn't degrade the epistemics.

### Phase 4 — Transport B: Reticulum sidecar

`reticulum-sidecar/` compose service on RetiNet/CE, LXMF send + inbox, transport
selection in `peer_send`. Start with Reticulum's `TCPClientInterface` pointed at
a *tailnet address* — this exercises the whole stack with zero radio hardware,
and swapping in an `RNodeInterface` later touches no brain code.

**Exit criterion:** brain A asserts a claim while brain B is *stopped*; brain B
receives it on next start via a propagation node.

### Phase 5 — Evaluate iroh (optional)

[iroh](https://docs.iroh.computer/what-is-iroh) reached 1.0 in June 2026:
Rust-native QUIC peer-to-peer, hole punching with relay fallback (~90% success,
comparable to WebRTC in practice), and — the relevant part — **node identity is
an Ed25519 public key** (`EndpointId`, formerly `NodeId`), authenticated so it
cannot be impersonated. That is the same identity-as-address model as Reticulum
destination hashes, so one identity concept spans both transports and the
envelope design carries over unchanged. QUIC's independent streams also mean a
bulk snapshot transfer won't head-of-line-block a claim stream.

`tonic-iroh-transport` (0.9.2, May 2026) runs tonic services directly over iroh
connections — typed protobuf contracts over p2p QUIC. Pre-1.0 with nine breaking
changes across sixteen releases; treat as promising, not settled.

Caveat from its own docs, which reinforces § 0: iroh's DHT discovery is *"a
bootstrap mechanism, not a trust anchor... the shared DHT buckets are still
globally writable and can be spammed or overwritten."* The envelope signature is
what makes that acceptable.

**Exit criterion:** a decision, recorded here, on whether iroh replaces or
supplements Transport A's Tailscale dependency.

---

## 9. Open questions

- **Peer authorization.** `peers/*.yaml` is a manual allowlist. Is there ever an
  auto-discovery path, and if so what stops a hostile peer from being added?
  Current answer: no auto-discovery. Revisit only with a concrete need.
- **Conflict resolution beyond `disputed`.** § 5.4 preserves both assertions.
  Does anything ever need to *resolve* them, or is `disputed` the terminal state?
- **Snapshot sync semantics.** `SnapshotService` restore uses MERGE, which is
  safe on a non-empty graph — but does a peer's snapshot merge into the local
  graph, or land in a quarantined subgraph first? Leaning quarantine.
- **Does RetiNet ship on PyPI?** It is Codeberg-hosted; packaging story unverified.
- **Rate limits and quota.** A peer whose task offers or claim volume exhausts the
  local LLM quota is a denial-of-service by accident. Where does the limit live —
  the skill, the queue, or the sidecar?
- **Key rotation.** If a brain's keypair is compromised or rotated, what happens
  to the months of `asserted_by` history pointing at the old pubkey?

---

## 10. Sources

- Reticulum: [manual](https://reticulum.network/manual/understanding.html) ·
  [license](https://reticulum.network/license.html) ·
  [license primer](https://github.com/markqvist/Reticulum/discussions/1062) ·
  [FOSDEM 2026 community meetup](https://fosdem.org/2026/schedule/event/9NCWUR-reticulum_community_meetup_implementations_migration_and_future/)
- LXMF: [protocol](https://github.com/markqvist/LXMF) ·
  [propagation nodes](https://reticulum.miraheze.org/wiki/Propagation_nodes)
- Implementations: [Reticulum-rs](https://github.com/BeechatNetworkSystemsLtd/Reticulum-rs) ·
  [Reticulum_CE](https://github.com/Reticulum-Community/Reticulum_CE) ·
  [awesome-reticulum](https://github.com/lorien/awesome-reticulum)
- [iroh](https://docs.iroh.computer/what-is-iroh) ·
  [tonic-iroh-transport](https://lib.rs/crates/tonic-iroh-transport) ·
  [tonic](https://lib.rs/crates/tonic)
