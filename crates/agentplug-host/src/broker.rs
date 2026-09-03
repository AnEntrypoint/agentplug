use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Mutex, OnceLock};

/// Cordis paper Section 6.2, "Service Multiplexing": a service broker is a
/// central entrypoint injected by both backing providers and consumers so
/// multiple providers coexist, with the broker dispatching each request
/// among them. This is the second of the two multiplicity forms the paper
/// names for a single service interface -- the first, exclusive binding
/// (at most one provider bound at a time, switching perturbs every
/// consumer), is what `registry.rs`'s `SharedPluginPool`/`get_active_provider`
/// already implements (Definition 45/46, `provider_k`/`target_n`/`quiet`).
/// This module adds the broker form ALONGSIDE it -- it never replaces
/// exclusive binding, which remains the default multiplicity for every
/// existing shared-plugin pool. A capability only gets broker semantics
/// once a caller explicitly registers >=2 named provider instances for it
/// via `register_provider`.
///
/// The paper names three capabilities the broker underlies: load balancing,
/// rolling updates, and cross-process invocation. Cross-process invocation
/// requires an RPC bridge across a real process boundary (the paper's
/// caveat: "must be designed against an asynchronous contract" since it
/// "incurs latency and may fail mid-flight") -- agentplug's plugins are
/// already in-process wasm guests dispatched synchronously through
/// `dispatch_on`, so that third capability has no analog here yet and is
/// intentionally out of this module's scope (tracked as a separate PRD row
/// if a real cross-process transport is ever added). This module implements
/// the two capabilities that DO have a direct analog: load balancing and
/// rolling updates over in-process provider instances.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoadBalancePolicy {
    #[default]
    RoundRobin,
    LeastLoaded,
}

/// One provider registered with a broker for a given service key. `weight`
/// is the paper's "selection weight" a rolling update adjusts gradually
/// (0..=100); `draining` marks a provider a rolling update is retiring --
/// it still answers in-flight requests but is never selected for a new one.
struct BrokerProvider {
    provider_id: String,
    weight: u32,
    draining: bool,
    in_flight: AtomicU64,
    total_dispatched: AtomicU64,
}

impl BrokerProvider {
    fn new(provider_id: String, weight: u32) -> Self {
        Self { provider_id, weight, draining: false, in_flight: AtomicU64::new(0), total_dispatched: AtomicU64::new(0) }
    }
}

struct ServiceBroker {
    policy: LoadBalancePolicy,
    providers: Vec<BrokerProvider>,
    round_robin_current: Mutex<HashMap<String, i64>>,
}

impl ServiceBroker {
    fn new(policy: LoadBalancePolicy) -> Self {
        Self { policy, providers: Vec::new(), round_robin_current: Mutex::new(HashMap::new()) }
    }

    fn routable_indices(&self) -> Vec<usize> {
        self.providers
            .iter()
            .enumerate()
            .filter(|(_, p)| !p.draining && p.weight > 0)
            .map(|(i, _)| i)
            .collect()
    }

    /// Selects the provider index for the next request per the broker's
    /// configured policy (paper: "a configurable policy, e.g. round-robin,
    /// least-loaded, latency-weighted"). Draining or zero-weight providers
    /// are never selected for new work, matching the rolling-update
    /// contract: "traffic is gradually shifted ... and the old providers
    /// are unloaded once they no longer carry in-flight requests."
    fn select(&self) -> Option<usize> {
        let routable = self.routable_indices();
        if routable.is_empty() {
            return None;
        }
        match self.policy {
            LoadBalancePolicy::RoundRobin => {
                // Smooth weighted round-robin (as used by nginx/LVS): each provider
                // carries a running `current` counter, incremented by its own weight
                // every tick; the tick's winner is whichever provider has the
                // highest `current`, which then has the total weight subtracted.
                // This interleaves selections proportional to weight (e.g. weights
                // 100/100 alternate every call; weights 300/100 select the heavy
                // provider 3 times per 1 of the light one) instead of dwelling on
                // one provider for a whole contiguous run before advancing, which a
                // plain cumulative-sum-over-a-modulo-cursor scan would do.
                if routable.is_empty() {
                    return None;
                }
                let total_weight: i64 = routable.iter().map(|&i| self.providers[i].weight as i64).sum();
                if total_weight == 0 {
                    return None;
                }
                let mut currents = self.round_robin_current.lock().unwrap_or_else(|e| e.into_inner());
                let mut best: Option<(usize, i64)> = None;
                for &i in &routable {
                    let entry = currents.entry(self.providers[i].provider_id.clone()).or_insert(0);
                    *entry += self.providers[i].weight as i64;
                    if best.map(|(_, c)| *entry > c).unwrap_or(true) {
                        best = Some((i, *entry));
                    }
                }
                let (winner, _) = best.expect("routable is non-empty, loop always sets best");
                if let Some(c) = currents.get_mut(&self.providers[winner].provider_id) {
                    *c -= total_weight;
                }
                Some(winner)
            }
            LoadBalancePolicy::LeastLoaded => routable
                .into_iter()
                .min_by_key(|&i| (self.providers[i].in_flight.load(AtomicOrdering::Relaxed), self.providers[i].weight == 0)),
        }
    }
}

static BROKERS: OnceLock<Mutex<HashMap<String, ServiceBroker>>> = OnceLock::new();

fn brokers() -> &'static Mutex<HashMap<String, ServiceBroker>> {
    BROKERS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Registers a provider instance under a service key, per the paper: "each
/// provider registers with the broker through a revertible effect, so
/// unloading it reverts the registration and drops it from the broker's
/// routing set automatically." Idempotent per `(service_key, provider_id)`.
/// A fresh service key defaults to round-robin; pass `policy` to set it
/// explicitly on first registration (subsequent calls ignore the argument,
/// use `set_policy` to change it later).
pub fn register_provider(service_key: &str, provider_id: &str, policy: LoadBalancePolicy) {
    register_provider_with_weight(service_key, provider_id, policy, 100);
}

/// Same as `register_provider`, but with an explicit initial weight. A
/// rolling update's incoming provider registers at weight 0 (see
/// `begin_rolling_update`) so it joins the routing set already-registered
/// but not yet receiving traffic, matching the paper's "once it becomes
/// ACTIVE, traffic is gradually shifted" -- registration and traffic
/// admission are separate events, not the same one.
pub fn register_provider_with_weight(service_key: &str, provider_id: &str, policy: LoadBalancePolicy, initial_weight: u32) {
    let mut guard = brokers().lock().unwrap_or_else(|e| e.into_inner());
    let broker = guard.entry(service_key.to_string()).or_insert_with(|| ServiceBroker::new(policy));
    if !broker.providers.iter().any(|p| p.provider_id == provider_id) {
        broker.providers.push(BrokerProvider::new(provider_id.to_string(), initial_weight.min(100)));
    }
}

/// Registers a rolling update's incoming provider at weight 0 -- the
/// paper's "the new provider is loaded as an additional fiber and
/// registers with the broker; once it becomes ACTIVE, traffic is gradually
/// shifted". Call this once the new provider instance is ACTIVE, then
/// drive the transition forward with repeated `shift_traffic` calls.
pub fn begin_rolling_update(service_key: &str, incoming_provider_id: &str) {
    register_provider_with_weight(service_key, incoming_provider_id, LoadBalancePolicy::RoundRobin, 0);
}

/// Reverts a provider's registration -- the inverse of `register_provider`,
/// invoked when the underlying plugin instance unloads. Refuses to drop a
/// provider that still carries in-flight requests (mirrors the rolling-
/// update contract: "old providers are unloaded once they no longer carry
/// in-flight requests"), returning `false` in that case so the caller can
/// retry once drained instead of silently losing routing state under load.
pub fn unregister_provider(service_key: &str, provider_id: &str) -> bool {
    let mut guard = brokers().lock().unwrap_or_else(|e| e.into_inner());
    let Some(broker) = guard.get_mut(service_key) else { return true };
    let Some(pos) = broker.providers.iter().position(|p| p.provider_id == provider_id) else { return true };
    if broker.providers[pos].in_flight.load(AtomicOrdering::Relaxed) > 0 {
        return false;
    }
    broker.providers.remove(pos);
    if broker.providers.is_empty() {
        guard.remove(service_key);
    }
    true
}

pub fn set_policy(service_key: &str, policy: LoadBalancePolicy) {
    let mut guard = brokers().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(broker) = guard.get_mut(service_key) {
        broker.policy = policy;
    }
}

/// A dispatch lease over the selected provider: `in_flight` is incremented
/// on selection and decremented on drop, so `LeastLoaded` reflects genuine
/// concurrent load and a draining provider's in-flight count is always
/// observable to `finish_drain`/`unregister_provider`.
pub struct RouteLease {
    service_key: String,
    pub provider_id: String,
}

impl Drop for RouteLease {
    fn drop(&mut self) {
        let guard = brokers().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(broker) = guard.get(&self.service_key) {
            if let Some(p) = broker.providers.iter().find(|p| p.provider_id == self.provider_id) {
                p.in_flight.fetch_sub(1, AtomicOrdering::Relaxed);
            }
        }
    }
}

/// Selects a provider for the next dispatch against `service_key` per the
/// broker's configured load-balancing policy. Returns `None` when the
/// service key has no registered broker (exclusive-binding path handles
/// it instead) or every registered provider is currently draining/zero-
/// weight (rolling update mid-cutover with no routable target left,
/// which should not happen under a correct `shift_traffic` sequence but
/// is handled as "no route" rather than a panic).
pub fn route(service_key: &str) -> Option<RouteLease> {
    let guard = brokers().lock().unwrap_or_else(|e| e.into_inner());
    let broker = guard.get(service_key)?;
    let idx = broker.select()?;
    let provider = &broker.providers[idx];
    provider.in_flight.fetch_add(1, AtomicOrdering::Relaxed);
    provider.total_dispatched.fetch_add(1, AtomicOrdering::Relaxed);
    Some(RouteLease { service_key: service_key.to_string(), provider_id: provider.provider_id.clone() })
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ProviderStatus {
    pub provider_id: String,
    pub weight: u32,
    pub draining: bool,
    pub in_flight: u64,
    pub total_dispatched: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct BrokerStatus {
    pub service_key: String,
    pub policy: LoadBalancePolicy,
    pub providers: Vec<ProviderStatus>,
}

pub fn status(service_key: &str) -> Option<BrokerStatus> {
    let guard = brokers().lock().unwrap_or_else(|e| e.into_inner());
    let broker = guard.get(service_key)?;
    Some(BrokerStatus {
        service_key: service_key.to_string(),
        policy: broker.policy,
        providers: broker
            .providers
            .iter()
            .map(|p| ProviderStatus {
                provider_id: p.provider_id.clone(),
                weight: p.weight,
                draining: p.draining,
                in_flight: p.in_flight.load(AtomicOrdering::Relaxed),
                total_dispatched: p.total_dispatched.load(AtomicOrdering::Relaxed),
            })
            .collect(),
    })
}

/// Rolling update, paper Section 6.2: "the new provider is loaded as an
/// additional fiber and registers with the broker; once it becomes ACTIVE,
/// traffic is gradually shifted from the old providers to the new one (e.g.
/// by adjusting selection weights), and the old providers are unloaded once
/// they no longer carry in-flight requests." This function performs one
/// discrete weight-shift step, not the whole transition atomically -- the
/// caller (the orchestrator driving the rollout) invokes it repeatedly
/// (e.g. on a timer or a health-check tick) to ramp `to_provider_id`'s
/// weight up by `step_percent` while ramping every OTHER non-draining
/// provider down proportionally, until the incoming provider reaches 100
/// and every other provider reaches 0 and is marked draining.
///
/// Returns `true` once the shift has fully completed (incoming provider at
/// weight 100, every other provider at weight 0 and draining) so the
/// caller knows it is safe to start calling `unregister_provider` on the
/// drained providers as their in-flight counts reach zero.
pub fn shift_traffic(service_key: &str, to_provider_id: &str, step_percent: u32) -> bool {
    let mut guard = brokers().lock().unwrap_or_else(|e| e.into_inner());
    let Some(broker) = guard.get_mut(service_key) else { return false };
    let step = step_percent.min(100);
    let Some(target_idx) = broker.providers.iter().position(|p| p.provider_id == to_provider_id) else { return false };

    let others: Vec<usize> = (0..broker.providers.len()).filter(|&i| i != target_idx).collect();
    let total_other_weight_before: u32 = others.iter().map(|&i| broker.providers[i].weight).sum();
    let reduction = step.min(total_other_weight_before);

    if reduction > 0 && total_other_weight_before > 0 {
        let mut remaining_reduction = reduction;
        for (n, &i) in others.iter().enumerate() {
            let share = if n + 1 == others.len() {
                remaining_reduction
            } else {
                (reduction as u64 * broker.providers[i].weight as u64 / total_other_weight_before as u64) as u32
            };
            let share = share.min(broker.providers[i].weight).min(remaining_reduction);
            broker.providers[i].weight -= share;
            remaining_reduction -= share;
            if broker.providers[i].weight == 0 {
                broker.providers[i].draining = true;
            }
        }
    }

    let new_target_weight = (broker.providers[target_idx].weight + reduction).min(100);
    broker.providers[target_idx].weight = new_target_weight;
    broker.providers[target_idx].draining = false;

    let others_all_zero = others.iter().all(|&i| broker.providers[i].weight == 0);
    others_all_zero && broker.providers[target_idx].weight >= 100
}

/// Prunes drained providers (weight 0, draining, zero in-flight) from a
/// broker's routing set -- called after `shift_traffic` reports completion
/// and the corresponding plugin instances have actually unloaded. Returns
/// the provider ids actually removed, so the caller can confirm which
/// underlying plugin instances are now safe to tear down.
pub fn reap_drained(service_key: &str) -> Vec<String> {
    let mut guard = brokers().lock().unwrap_or_else(|e| e.into_inner());
    let Some(broker) = guard.get_mut(service_key) else { return Vec::new() };
    let mut removed = Vec::new();
    broker.providers.retain(|p| {
        let drop_it = p.draining && p.weight == 0 && p.in_flight.load(AtomicOrdering::Relaxed) == 0;
        if drop_it {
            removed.push(p.provider_id.clone());
        }
        !drop_it
    });
    removed
}
