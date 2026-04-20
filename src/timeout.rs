use crate::env::TIMEOUT;
use crate::orchestrator::Orchestrator;
use crate::services::changes::{ServiceChange, apply_changes};
use crate::services::service_info::ServiceInfo;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Notify, RwLock};

pub(crate) async fn check_timeouts(
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
        apply_timeouts(&mut services_mut, &orchestrator).await;
    }
}

pub(crate) async fn apply_timeouts(
    services: &mut HashMap<String, ServiceInfo>,
    orchestrator: &Orchestrator,
) {
    let changes = collect_timed_out_clients(services);
    if !changes.is_empty() {
        apply_changes(changes, services, None, orchestrator).await;
    }
}

fn collect_timed_out_clients(services: &HashMap<String, ServiceInfo>) -> Vec<ServiceChange> {
    let mut changes = Vec::new();

    for (name, si) in services {
        let Some(timeout) = si.timeout() else {
            continue;
        };
        if timeout == 0 {
            continue;
        }
        let ServiceInfo::Registered(reg) = si else {
            continue;
        };

        for client in reg.expired_proxy_clients(Duration::from_secs(timeout)) {
            changes.push(ServiceChange::ProxyClientTimedOut {
                name: name.clone(),
                client,
            });
        }
    }

    // Backend-triggered chain entries: stored on the initiator's first dep.
    // Timeout comes from the initiator's configured entry timeout.
    let now = Instant::now();
    for si in services.values() {
        let ServiceInfo::Registered(reg) = si else {
            continue;
        };
        for (client, latest) in reg.backend_entry_clients() {
            let initiator_name = client.name();
            let Some(timeout) = services.get(initiator_name).and_then(ServiceInfo::timeout) else {
                continue;
            };
            if timeout == 0 {
                continue;
            }
            if now.duration_since(latest) < Duration::from_secs(timeout) {
                continue;
            }
            let Some((initiator_ip, initiator_docker)) = client.replica_identity() else {
                continue;
            };
            changes.push(ServiceChange::BackendChainTimedOut {
                initiator_name: initiator_name.to_string(),
                initiator_ip,
                initiator_docker: initiator_docker.map(String::from),
            });
        }
    }

    changes
}

fn nearest_timeout(services: &HashMap<String, ServiceInfo>) -> Duration {
    let mut nearest = Duration::from_secs(*TIMEOUT);

    for si in services.values() {
        let Some(timeout) = si.timeout() else {
            continue;
        };
        if timeout == 0 {
            continue;
        }

        let timeout_duration = Duration::from_secs(timeout);

        // cap by the configured timeout so new clients are caught within one period
        nearest = nearest.min(timeout_duration);

        if let ServiceInfo::Registered(reg) = si
            && let Some(expiry) = reg.nearest_proxy_expiry(timeout_duration)
        {
            nearest = nearest.min(expiry);
        }
    }

    // backend-triggered chain entries: timeout is the initiator's configured
    // entry timeout, not the dep service's.
    let now = Instant::now();
    for si in services.values() {
        let ServiceInfo::Registered(reg) = si else {
            continue;
        };
        for (client, latest) in reg.backend_entry_clients() {
            let Some(timeout) = services.get(client.name()).and_then(ServiceInfo::timeout) else {
                continue;
            };
            if timeout == 0 {
                continue;
            }
            let timeout_duration = Duration::from_secs(timeout);
            let elapsed = now.duration_since(latest);
            let expiry = timeout_duration.saturating_sub(elapsed);
            nearest = nearest.min(expiry);
        }
    }

    nearest
}
