//! Provider concurrency gate — Phase 4 of `project-docs/PROVIDER_ROUTING_PLAN.md`.
//!
//! One gate per provider, sized from the registry's `limits.concurrency`, shared
//! by everything that calls a model: the queue's background jobs, the chat
//! loops, sub-agents, synthesis and the distiller. A provider with no declared
//! concurrency is not gated.
//!
//! **The permit covers one request, never a job or a turn.** A chat turn holds
//! its slot only while its request streams, and releases it before running
//! tools. Holding it across tools would deadlock a one-slot provider (Ollama
//! Cloud free = 1 concurrent request): a turn that delegates to a sub-agent
//! falling back to the same provider would wait on its own slot. For the same
//! reason the queue's job semaphores are not this gate — a job that took a
//! provider slot and then called the model would take it twice.
//!
//! **Interactive work is served first.** Waiters queue in three classes read
//! from the work's `ROUTE_CONTEXT`: chat, then sub-agents, then background.
//! Within a class, first come first served. llama.cpp would otherwise serve its
//! two slots in arrival order, so the front door waited behind background jobs.
//!
//! Waits are bounded without a timeout of their own: every holder is an HTTP
//! request with its own timeout (120 s cloud, `OLLAMA_LOCAL_TIMEOUT_SECS` local).
//! A waiter that gives up (an abandoned chat turn) is skipped without leaking a
//! slot. Embeddings are not gated: they are short, local, and gating them behind
//! generation would stall retrieval.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Instant;

use serde_json::{Value, json};
use tokio::sync::oneshot;
use tracing::{debug, info, warn};

use crate::provider_registry::{self, ProviderEntry};
use crate::routing::{Purpose, RouteContext};

/// A wait at least this long is logged at INFO; one at least
/// [`WARN_WAIT_MS`] at WARN. Both mean the provider is the bottleneck.
const INFO_WAIT_MS: u64 = 1_000;
const WARN_WAIT_MS: u64 = 30_000;

const CLASSES: [Purpose; 3] = [Purpose::Chat, Purpose::Subagent, Purpose::Background];

fn class_index(p: Purpose) -> usize {
    match p {
        Purpose::Chat => 0,
        Purpose::Subagent => 1,
        Purpose::Background => 2,
    }
}

#[derive(Default)]
struct Slot {
    capacity: usize,
    in_use: usize,
    /// One FIFO per priority class, highest priority first.
    waiters: [VecDeque<oneshot::Sender<GatePermit>>; 3],
    acquired: u64,
    waited: u64,
    max_wait_ms: u64,
}

#[derive(Default)]
struct State {
    slots: HashMap<String, Slot>,
}

/// Concurrency gates keyed by provider-registry name. Cheap to clone; clones
/// share state. The process-wide instance is reached through the free
/// functions below; tests build their own.
#[derive(Clone, Default)]
pub struct ProviderGate {
    state: Arc<Mutex<State>>,
}

/// One held slot. Dropping it frees the slot, handing it straight to the
/// highest-priority waiter.
pub struct GatePermit {
    gate: ProviderGate,
    provider: String,
    armed: bool,
}

impl Drop for GatePermit {
    fn drop(&mut self) {
        if self.armed {
            self.armed = false;
            self.gate.release(&self.provider);
        }
    }
}

impl ProviderGate {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Size the gates from a provider table. A provider with no declared
    /// concurrency is ungated: its slot is removed, and anyone waiting on it
    /// proceeds ungated. A limit of 0 is read as 1 — a gate nobody can pass
    /// would hang every call to that provider.
    pub fn configure(&self, table: &BTreeMap<String, ProviderEntry>) {
        let mut st = self.lock();
        let wanted: HashMap<String, usize> = table
            .iter()
            .filter_map(|(name, e)| {
                e.limits
                    .concurrency
                    .map(|c| (provider_registry::canonical_name(name), c.max(1) as usize))
            })
            .collect();
        st.slots.retain(|name, _| wanted.contains_key(name));
        for (name, capacity) in wanted {
            let slot = st.slots.entry(name.clone()).or_default();
            slot.capacity = capacity;
            self.dispatch(&name, slot);
        }
    }

    /// Wait for a slot on `provider`, queued by `purpose`. `None` means the
    /// provider is not gated (or stopped being gated while this call waited),
    /// so the call proceeds without a permit.
    pub async fn acquire(&self, provider: &str, purpose: Purpose) -> Option<GatePermit> {
        let rx = {
            let mut st = self.lock();
            let slot = st.slots.get_mut(provider)?;
            slot.acquired += 1;
            if slot.in_use < slot.capacity && slot.waiters.iter().all(VecDeque::is_empty) {
                slot.in_use += 1;
                return Some(GatePermit {
                    gate: self.clone(),
                    provider: provider.to_string(),
                    armed: true,
                });
            }
            let (tx, rx) = oneshot::channel();
            slot.waiters[class_index(purpose)].push_back(tx);
            slot.waited += 1;
            rx
        };
        let started = Instant::now();
        let permit = rx.await.ok();
        let waited_ms = started.elapsed().as_millis() as u64;
        if let Some(slot) = self.lock().slots.get_mut(provider) {
            slot.max_wait_ms = slot.max_wait_ms.max(waited_ms);
        }
        permit
    }

    /// Free one slot and hand it to the next waiter.
    fn release(&self, provider: &str) {
        let mut st = self.lock();
        // A provider no longer gated has nothing left to track.
        let Some(slot) = st.slots.get_mut(provider) else {
            return;
        };
        slot.in_use = slot.in_use.saturating_sub(1);
        self.dispatch(provider, slot);
    }

    /// Fill free capacity from the waiters, highest priority first. A waiter
    /// that has gone (its call was cancelled) is skipped: the permit it was
    /// handed comes back disarmed, so dropping it here does not re-enter
    /// `release` while the lock is held.
    fn dispatch(&self, provider: &str, slot: &mut Slot) {
        while slot.in_use < slot.capacity {
            let Some(tx) = slot.waiters.iter_mut().find_map(VecDeque::pop_front) else {
                break;
            };
            slot.in_use += 1;
            let permit = GatePermit {
                gate: self.clone(),
                provider: provider.to_string(),
                armed: true,
            };
            if let Err(mut unclaimed) = tx.send(permit) {
                unclaimed.armed = false;
                slot.in_use -= 1;
            }
        }
    }

    /// One provider's gate as JSON, or `null` when it is not gated.
    pub fn snapshot(&self, provider: &str) -> Value {
        let st = self.lock();
        let Some(slot) = st.slots.get(provider) else {
            return Value::Null;
        };
        let waiting: serde_json::Map<String, Value> = CLASSES
            .iter()
            .map(|p| {
                (
                    p.as_str().to_string(),
                    json!(slot.waiters[class_index(*p)].len()),
                )
            })
            .collect();
        json!({
            "capacity": slot.capacity,
            "in_use": slot.in_use,
            "waiting": waiting,
            "acquired_total": slot.acquired,
            "waited_total": slot.waited,
            "max_wait_ms": slot.max_wait_ms,
        })
    }
}

// ── Process-wide gate ───────────────────────────────────────────────────────

fn global() -> &'static ProviderGate {
    static GATE: OnceLock<ProviderGate> = OnceLock::new();
    GATE.get_or_init(ProviderGate::default)
}

/// Size the process-wide gates. Called whenever the provider registry is
/// installed (startup and `reload_models`).
pub fn configure(table: &BTreeMap<String, ProviderEntry>) {
    global().configure(table);
}

/// Wait for a slot on `provider` for the work running on this task
/// (`ROUTE_CONTEXT`: chat first, then sub-agents, then background). Hold the
/// returned permit for exactly one request.
pub async fn acquire(provider: &str) -> Option<GatePermit> {
    let name = provider_registry::canonical_name(provider);
    let purpose = RouteContext::current().purpose;
    let started = Instant::now();
    let permit = global().acquire(&name, purpose).await;
    let waited_ms = started.elapsed().as_millis() as u64;
    if waited_ms >= WARN_WAIT_MS {
        warn!(provider = %name, purpose = %purpose, waited_ms, "waited for a provider concurrency slot");
    } else if waited_ms >= INFO_WAIT_MS {
        info!(provider = %name, purpose = %purpose, waited_ms, "waited for a provider concurrency slot");
    } else if waited_ms > 0 {
        debug!(provider = %name, purpose = %purpose, waited_ms, "waited for a provider concurrency slot");
    }
    permit
}

/// One provider's gate as JSON for `/api/llm/budget`, or `null` when ungated.
pub fn snapshot_json(provider: &str) -> Value {
    global().snapshot(&provider_registry::canonical_name(provider))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn table(pairs: &[(&str, u32)]) -> BTreeMap<String, ProviderEntry> {
        pairs
            .iter()
            .map(|(name, c)| {
                let e: ProviderEntry = serde_yaml::from_str(&format!(
                    "kind: openai_compat\nbilling: free\nbase_url_default: x\nlimits: {{ concurrency: {c} }}"
                ))
                .unwrap();
                (name.to_string(), e)
            })
            .collect()
    }

    fn gate(pairs: &[(&str, u32)]) -> ProviderGate {
        let g = ProviderGate::default();
        g.configure(&table(pairs));
        g
    }

    /// Whether `fut` is still waiting after the scheduler has had a chance to run it.
    async fn still_waiting<F: std::future::Future>(fut: F) -> bool {
        tokio::time::timeout(Duration::from_millis(50), fut)
            .await
            .is_err()
    }

    async fn wait_until_queued(g: &ProviderGate, provider: &str, class: &str, n: u64) {
        for _ in 0..200 {
            if g.snapshot(provider)["waiting"][class].as_u64() == Some(n) {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("{class} waiter never queued on {provider}");
    }

    #[tokio::test]
    async fn an_undeclared_provider_is_not_gated() {
        let g = gate(&[("ollama-cloud", 1)]);
        assert!(g.acquire("gemini", Purpose::Background).await.is_none());
        assert!(g.snapshot("gemini").is_null());
    }

    #[tokio::test]
    async fn capacity_is_respected_and_a_release_hands_the_slot_on() {
        let g = gate(&[("ollama-cloud", 1)]);
        let first = g
            .acquire("ollama-cloud", Purpose::Chat)
            .await
            .expect("gated");
        assert!(still_waiting(g.acquire("ollama-cloud", Purpose::Chat)).await);

        let g2 = g.clone();
        let second = tokio::spawn(async move { g2.acquire("ollama-cloud", Purpose::Chat).await });
        wait_until_queued(&g, "ollama-cloud", "chat", 1).await;
        drop(first);
        let second = second.await.unwrap().expect("handed the slot");
        assert_eq!(g.snapshot("ollama-cloud")["in_use"], 1);
        drop(second);
        assert_eq!(g.snapshot("ollama-cloud")["in_use"], 0);
    }

    #[tokio::test]
    async fn chat_is_served_before_background_that_queued_first() {
        let g = gate(&[("llamacpp", 1)]);
        let held = g.acquire("llamacpp", Purpose::Background).await.unwrap();

        let gb = g.clone();
        let background =
            tokio::spawn(async move { gb.acquire("llamacpp", Purpose::Background).await });
        wait_until_queued(&g, "llamacpp", "background", 1).await;
        let gc = g.clone();
        let chat = tokio::spawn(async move { gc.acquire("llamacpp", Purpose::Chat).await });
        wait_until_queued(&g, "llamacpp", "chat", 1).await;

        drop(held);
        let chat_permit = chat.await.unwrap().expect("chat served first");
        assert!(
            !background.is_finished(),
            "background must still be waiting"
        );
        drop(chat_permit);
        assert!(background.await.unwrap().is_some());
    }

    /// An abandoned chat turn must not strand the slot it was about to get.
    #[tokio::test]
    async fn a_cancelled_waiter_does_not_leak_a_slot() {
        let g = gate(&[("ollama-cloud", 1)]);
        let held = g
            .acquire("ollama-cloud", Purpose::Background)
            .await
            .unwrap();
        assert!(still_waiting(g.acquire("ollama-cloud", Purpose::Chat)).await);
        assert_eq!(g.snapshot("ollama-cloud")["waiting"]["chat"], 1);

        drop(held);
        assert_eq!(
            g.snapshot("ollama-cloud")["in_use"],
            0,
            "the dead waiter was skipped"
        );
        let again = tokio::time::timeout(
            Duration::from_millis(200),
            g.acquire("ollama-cloud", Purpose::Background),
        )
        .await
        .expect("slot free, not leaked");
        assert!(again.is_some());
    }

    #[tokio::test]
    async fn raising_the_limit_wakes_waiters() {
        let g = gate(&[("ollama-cloud", 1)]);
        let _held = g.acquire("ollama-cloud", Purpose::Chat).await.unwrap();
        let gw = g.clone();
        let waiter = tokio::spawn(async move { gw.acquire("ollama-cloud", Purpose::Chat).await });
        wait_until_queued(&g, "ollama-cloud", "chat", 1).await;

        g.configure(&table(&[("ollama-cloud", 3)])); // e.g. a Pro key
        // Bound to a name: a temporary permit would be released before the count.
        let woken = waiter.await.unwrap();
        assert!(woken.is_some());
        assert_eq!(g.snapshot("ollama-cloud")["in_use"], 2);
    }

    #[tokio::test]
    async fn dropping_a_limit_releases_waiters_ungated() {
        let g = gate(&[("mistral", 1)]);
        let held = g.acquire("mistral", Purpose::Chat).await.unwrap();
        let gw = g.clone();
        let waiter = tokio::spawn(async move { gw.acquire("mistral", Purpose::Chat).await });
        wait_until_queued(&g, "mistral", "chat", 1).await;

        g.configure(&table(&[]));
        assert!(waiter.await.unwrap().is_none(), "proceeds without a permit");
        drop(held); // releasing into a removed gate is a no-op, not a panic
        assert!(g.snapshot("mistral").is_null());
    }

    #[tokio::test]
    async fn a_declared_limit_of_zero_still_lets_calls_through_one_at_a_time() {
        let g = gate(&[("x", 0)]);
        assert_eq!(g.snapshot("x")["capacity"], 1);
        assert!(g.acquire("x", Purpose::Chat).await.is_some());
    }

    #[test]
    fn the_checked_in_registry_gates_the_providers_with_a_real_concurrency_limit() {
        // Ollama Cloud free = "1 concurrent request" (ollama.com/pricing,
        // checked 2026-09-23); llama.cpp = its `-np 2` slots.
        let catalog = crate::model_config::ModelCatalog::load(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../models.yaml"),
        )
        .expect("models.yaml parses");
        let g = ProviderGate::default();
        g.configure(&provider_registry::merged(&catalog.providers));
        assert_eq!(g.snapshot("ollama-cloud")["capacity"], 1);
        assert_eq!(g.snapshot("llamacpp")["capacity"], 2);
        assert!(
            g.snapshot("gemini").is_null(),
            "no declared concurrency, not gated"
        );
    }
}
