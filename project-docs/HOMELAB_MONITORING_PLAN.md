# Home-lab monitoring plan

Prometheus and Grafana watch the machines; the brain reads what they record.
Planned 2026-10-26 after the owner asked whether Grafana makes sense for the
home lab. The stack lives outside this repo, in `~/homelab/monitoring/` beside
the Pi-hole (`~/homelab/pihole/`) and ntfy (`~/homelab/ntfy/`); its
`README.md` holds the setup steps. Owner decisions 2026-10-27 are in §9.

## 1. Why

Nothing watches the hardware. The brain's DuckDB telemetry and the HBI cover
what the brain does (jobs, providers, spend), not the box it runs on, so these
questions are answered by guessing today:

- **The GPU is the tightest limit.** 8 GB is shared by the llama.cpp sidecar
  (Gemma-4-E4B, ~3.3 GB plus KV), Whisper bursts and fine-tuning runs, with
  ~820 MB free at the tightest (dev note `llamacpp`). When a local job fails
  with an OOM or a 5xx, nothing records what else held VRAM at that moment.
- **The host is busy.** 32 cores and 78 GB, with ~44 GB in use; ml-runner
  takes 12 cores overnight. Which container is growing, and how many days
  until the disk (160 GB free on 2026-10-26) runs out, are unknown.
- **The home-lab goal** (owner, 2026-10-16; todo 8b29f766) is a brain that
  finds lab services over the tailnet. Prometheus's target list is an
  inventory of exactly that, with up/down for each.

## 2. What is not in it

- **No Grafana over the brain's DuckDB.** DuckDB allows one process per file
  while one writes, and the brain holds `telemetry.duckdb` open the whole time.
  A DuckDB datasource would fail to open it or break the brain's writes. Brain
  numbers reach Grafana through the brain's own `/metrics` (Phase 3).
- **No Neo4j datasource.** The community plugin is weak, and the graph's
  questions belong to `lookup` views.
- **No copy of HBI panels.** The HBI is the view of the brain; Grafana is the
  view of the infrastructure. Two dashboards over the same data drift apart.
- **No Loki yet.** `docker compose logs` covers today's needs; revisit if a
  question needs logs across machines.

## 3. Phase 0: the stack (owner runs it, no brain code)

`~/homelab/monitoring/` is its own compose project (`homelab-monitoring`),
outside this repo, so the brain's post-commit rebuild never restarts it and it
keeps recording while the brain is down.

**Machines** (owner, 2026-10-27): pcmain (this host) and Gizmatron (the robot,
a Raspberry Pi) now; the three laptops later (two are on the tailnet,
gadgetlab and garagelaptop; the third is not yet). Each target carries
`machine` and `role` (`server`, `robot`, `laptop`); laptops never alert as
down, since they sleep and travel. The Pi runs Debian's
`prometheus-node-exporter` bound to its tailnet address.

| Service | Image | Listens on | Job |
|---|---|---|---|
| prometheus | `prom/prometheus:v3.6.0` | tailnet IP :9090 | scrapes every 15 s, keeps 90 d / 20 GB |
| grafana | `grafana/grafana:12.2.0` | tailnet IP :3000 | dashboards, alerts later |
| node-exporter | `prom/node-exporter:v1.9.1` | 127.0.0.1:9100 | host CPU, memory, disk, network |
| gpu-exporter | `utkuozdemir/nvidia_gpu_exporter:1.3.2` | 127.0.0.1:9835 | VRAM, utilisation, temperature, power |
| cadvisor | `gcr.io/cadvisor/cadvisor:v0.52.1` | 127.0.0.1:8089 | per-container CPU and memory |
| home-assistant job | (HA's `prometheus:` integration) | 127.0.0.1:8123 | every HA entity, Pi-hole's sensors included |
| ntfy (`~/homelab/ntfy`) | `binwiederhier/ntfy:v2.14.0` | tailnet IP :2586 | alert pushes to the phone |

Choices:

- **Host network, one address per service.** Exporters bind loopback;
  Prometheus and Grafana bind only the tailnet address (never 0.0.0.0), so the
  LAN and the Mullvad exit never see them. gizmo-server already holds
  127.0.0.1:9090; the tailnet bind on the same port does not collide.
- **nvidia-smi exporter, not DCGM.** DCGM's exporter wants a newer driver than
  535; this one reads nvidia-smi through the nvidia runtime's `utility`
  capability and reserves no VRAM.
- **Tailnet addresses stay out of this repo.** They are in
  `~/homelab/monitoring/.env` and `prometheus/targets/*.yml`, which is not a
  git repo; `.example` files show the shape.
- **Targets by IP, not MagicDNS name:** Prometheus uses the host's resolver.
- **Grafana phones nothing home** (analytics, update checks, news feed off).
- **The Prometheus HTTP API stays read-only:** no `--web.enable-admin-api`, no
  `--web.enable-lifecycle`. That is what makes Phase 1 safe.

Steps are in `~/homelab/monitoring/README.md`: ntfy, Home Assistant (the
`prometheus:` integration, a long-lived token for Prometheus, the alert
automation), node_exporter on the Pi, then the stack, then dashboards 1860
(Node Exporter Full) and 14574 (NVIDIA GPU) exported to `grafana/dashboards/`.

Checked 2026-10-26: every image tag resolves, `docker compose config` and
`promtool check config` pass. Checked 2026-10-27 with throwaway containers:
Grafana loads the provisioned datasource, five rules and both receivers, and a
forced disk rule reached ntfy as "Lab: Disk low" (one line per filesystem) and
the Home Assistant webhook with the same title and message. Not yet started
for real.

**Exit:** a week of data; the VRAM graph shows llama.cpp and Whisper
separately enough to read a Whisper burst against an E4B request.

## 4. Phase 1: the brain reads lab state

- **A `promql` view kind** (`crates/host/src/views.rs`) beside `cypher` and
  `notes`: `views/<name>.yaml` with `{name, kind: promql, description, words,
  query, range?}`, in a new `homelab` plugin bundle. First views: `lab_gpu`
  (VRAM used/free, utilisation, temperature now), `lab_host` (load, memory,
  disk free and days-to-full at the week's rate), `lab_containers` (top
  containers by memory), `lab_targets` (what is scraped, up or down).
  Named queries, not free PromQL, for the reason in GRAPH_CONTEXT_PLAN Phase 1:
  compiled named queries beat a small model writing the query language
  (E4B 10/33 on free Cypher).
- **Where Prometheus is:** `PROMETHEUS_URL` in `.env` (it names a tailnet
  host) and that `host:port` in `HTTP_ALLOW_INTERNAL`, the net_policy's one
  way through, as with Home Assistant and `ml-runner:8000`. No key: Prometheus
  is reachable only on the tailnet and its API is read-only. The container
  already resolves MagicDNS (compose `dns:`, 2026-10-16); confirm that it
  reaches this same host's tailnet address before building.
- **Failures are visible.** A target that is down reads "unreachable since
  <time>", a query with no series reads "no data for <metric>", never 0. A
  value older than two scrape intervals is marked stale. Prometheus being down
  reads as that, and nothing else in the brain depends on it.
- **An `:ApiContext {name: 'prometheus', kind: device-state}`**, graph-only
  like the Home Assistant MCP server (no seed file, since the base URL is a
  tailnet host), so API_USE_PLAN's matcher sees it.

**Exit:** "how much VRAM is free?" and "is anything in the lab down?" are
answered in chat from a view, compared against Grafana.

## 5. Phase 2: GPU state where local work fails

Diagnosis first; no change to behaviour:

- When a local job (llama.cpp, Whisper, embeddings) fails with an OOM, a
  timeout or a 5xx, the failure record carries the GPU state at that moment
  (VRAM used, which processes, utilisation), read from Prometheus at the
  failure time.
- After two weeks, count the failures that coincide with contention. Only if
  the count is real does the scheduler get a VRAM-headroom check before it
  dispatches local work, and `gpu_lease.sh acquire` a "GPU is busy now"
  warning. If it is not, Phase 2 ends at the diagnosis.

## 6. Phase 3: the brain's own `/metrics`

Owner, 2026-10-27: probably yes, planned later; brain todo "Plan the brain's
/metrics endpoint". The sketch below is the starting point, not a decision.

- `GET /metrics` in Prometheus text format: queue depth by state and provider,
  jobs completed/failed/dead, provider calls and latency, spend against each
  cap, GPU lease held, scheduler paused, local concurrency slots in use.
- **Counts only, bounded labels:** provider, state, workflow name at most;
  never a task id, a prompt or note content. It is excluded from auth like
  `/health`; port 3001 is published on all interfaces, so nothing private may
  appear in it.
- llama.cpp's `--metrics` (slots, tokens/s, KV use) would need the sidecar
  published on loopback. That is a brain compose change for the owner to
  decide.
- Then uncomment the `agent-brain` job in `prometheus.yml`.

## 7. Phase 4: alerts and the weekly look

- **Grafana alerts**, provisioned with the stack in Phase 0 (owner,
  2026-10-27: to both ntfy and Home Assistant): a target down 15 min (not
  laptops), disk under 10% free, memory under 5% available, GPU over 85 °C,
  the robot's CPU over 80 °C. Each goes to self-hosted ntfy (topic
  `lab-alerts`, through ntfy's built-in `grafana` template) and to an HA
  webhook automation (the HA phone app plus a persistent notification). A
  Prometheus that stops answering fires Grafana's DatasourceError, not silence.
  ntfy is also a candidate channel for the brain's own notifications.
- **A weekly `lab-review` workflow** in the Home Lab area (PROGRAMS_REORG_PLAN
  §3) reads the week through Phase 1's range views and writes one note: disk
  days-to-full, hours of GPU contention, containers whose memory grew, anything
  that went down. It writes "nothing changed" when nothing did, and files a
  todo only for a trend.
- **Home Assistant's sensors** are scraped from Phase 0 (owner, 2026-10-27),
  so the weekly review can read house state beside the lab's.

## 8. Phase 5: the inventory

Prometheus's `/api/v1/targets` becomes the first inventory source for todo
8b29f766: every scraped machine and service is a known lab asset with its
up/down history. Discovery beyond what is scraped (tailscale status, which
services the brain may call) stays that todo's design question, not this
plan's.

## 9. Owner decisions

Answered 2026-10-27:

1. **Machines:** pcmain and Gizmatron now, the three laptops later (§3).
2. **Retention:** 90 days / 20 GB stands (not discussed; change in `.env`).
3. **Brain `/metrics`:** probably; planned later from a brain todo (§6).
   llama.cpp's metrics go with that todo.
4. **Alerts:** both ntfy and Home Assistant (§7, built in Phase 0).
5. **Home Assistant sensors:** yes, into Prometheus (§3).

Open: which laptop is the third, and whether the brain's notifications move to
ntfy too.
