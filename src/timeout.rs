use crate::constants::TIMEOUT;
use crate::orchestrator::Orchestrator;
use crate::services::changes::{ServiceChange, apply_changes};
use crate::services::service_info::ServiceInfo;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Notify, RwLock};

pub(crate) async fn check_proxy_timeouts(
    services: Arc<RwLock<HashMap<String, ServiceInfo>>>,
    orchestrator: Orchestrator,
    config_changed: Arc<Notify>,
) {
    loop {
        let sleep_duration = nearest_timeout(&*services.read().await);

        tokio::select! {
            () = tokio::time::sleep(sleep_duration) => {}
            () = config_changed.notified() => {}
        }

        let mut services_mut = services.write().await;
        let changes = collect_timed_out_clients(&services_mut);
        if !changes.is_empty() {
            apply_changes(changes, &mut services_mut, None, &orchestrator).await;
        }
    }
}

fn collect_timed_out_clients(services: &HashMap<String, ServiceInfo>) -> Vec<ServiceChange> {
    let now = Instant::now();
    let mut changes = Vec::new();

    for (name, si) in services {
        let Some(timeout) = si.is_proxy_reachable() else {
            continue;
        };
        if timeout == 0 {
            continue;
        }
        let ServiceInfo::Registered(reg) = si else {
            continue;
        };

        let timeout_duration = Duration::from_secs(timeout);
        for (client, ci) in reg.clients() {
            if client.is_proxy().is_none() {
                continue;
            }
            if now.duration_since(ci.latest()) >= timeout_duration {
                changes.push(ServiceChange::ProxyClientTimedOut {
                    name: name.clone(),
                    client: client.clone(),
                });
            }
        }
    }

    changes
}

fn nearest_timeout(services: &HashMap<String, ServiceInfo>) -> Duration {
    let now = Instant::now();
    let mut nearest = Duration::from_secs(*TIMEOUT);

    for si in services.values() {
        let Some(timeout) = si.is_proxy_reachable() else {
            continue;
        };
        if timeout == 0 {
            continue;
        }

        let timeout_duration = Duration::from_secs(timeout);

        // cap by the configured timeout so new clients are caught within one period
        nearest = nearest.min(timeout_duration);

        if let ServiceInfo::Registered(reg) = si {
            for (client, ci) in reg.clients() {
                if client.is_proxy().is_none() {
                    continue;
                }
                let remaining = timeout_duration.saturating_sub(now.duration_since(ci.latest()));
                nearest = nearest.min(remaining);
            }
        }
    }

    nearest
}
