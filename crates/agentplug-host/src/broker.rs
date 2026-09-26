use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Mutex, OnceLock};


#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoadBalancePolicy {
    #[default]
    RoundRobin,
    LeastLoaded,
}

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

    fn select(&self) -> Option<usize> {
        let routable = self.routable_indices();
        if routable.is_empty() {
            return None;
        }
        match self.policy {
            LoadBalancePolicy::RoundRobin => {
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

pub fn register_provider(service_key: &str, provider_id: &str, policy: LoadBalancePolicy) {
    register_provider_with_weight(service_key, provider_id, policy, 100);
}

pub fn register_provider_with_weight(service_key: &str, provider_id: &str, policy: LoadBalancePolicy, initial_weight: u32) {
    let mut guard = brokers().lock().unwrap_or_else(|e| e.into_inner());
    let broker = guard.entry(service_key.to_string()).or_insert_with(|| ServiceBroker::new(policy));
    if !broker.providers.iter().any(|p| p.provider_id == provider_id) {
        broker.providers.push(BrokerProvider::new(provider_id.to_string(), initial_weight.min(100)));
    }
}

pub fn begin_rolling_update(service_key: &str, incoming_provider_id: &str) {
    register_provider_with_weight(service_key, incoming_provider_id, LoadBalancePolicy::RoundRobin, 0);
}

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
