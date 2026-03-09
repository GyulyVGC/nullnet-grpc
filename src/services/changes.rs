use crate::orchestrator::Orchestrator;
use crate::services::service_info::ServiceInfo;
use std::collections::HashMap;
use std::net::IpAddr;

pub(crate) enum ServiceChange {
    /// Service removed from config entirely.
    Removed { name: String },
    /// Service dependencies changed in config.
    DepsChanged { name: String },
    /// Service `proxy_reachable` flag toggled in config.
    ReachabilityChanged { name: String },
    /// Host re-registered without this service.
    Unregistered { name: String },
    /// A proxy node disconnected; tear down its proxy chains.
    ProxyDisconnected { ip: IpAddr },
}

pub(crate) fn detect_config_changes(
    current: &HashMap<String, ServiceInfo>,
    loaded: &HashMap<String, ServiceInfo>,
) -> Vec<ServiceChange> {
    let mut changes = Vec::new();

    // services removed from config
    for name in current.keys() {
        if !loaded.contains_key(name) {
            changes.push(ServiceChange::Removed { name: name.clone() });
        }
    }

    // services with changed deps or reachability
    for (name, loaded_info) in loaded {
        if let Some(old_info) = current.get(name) {
            if loaded_info.dependencies() != old_info.dependencies() {
                changes.push(ServiceChange::DepsChanged { name: name.clone() });
            } else if loaded_info.is_proxy_reachable() != old_info.is_proxy_reachable() {
                changes.push(ServiceChange::ReachabilityChanged { name: name.clone() });
            }
        }
    }

    changes
}

pub(crate) fn detect_services_list_changes(
    current: &HashMap<String, ServiceInfo>,
    sender_ip: IpAddr,
    service_list: &[(String, u16)],
) -> Vec<ServiceChange> {
    current
        .iter()
        .filter_map(|(name, si)| {
            if let ServiceInfo::Registered(reg) = si
                && reg.ip_port().0 == sender_ip
                && !service_list.iter().any(|(s, _)| s == name)
            {
                return Some(ServiceChange::Unregistered { name: name.clone() });
            }
            None
        })
        .collect()
}

pub(crate) fn detect_node_disconnect_changes(
    current: &HashMap<String, ServiceInfo>,
    disconnected_ip: IpAddr,
) -> Vec<ServiceChange> {
    let mut changes: Vec<ServiceChange> = current
        .iter()
        .filter_map(|(name, si)| {
            if let ServiceInfo::Registered(reg) = si
                && reg.ip_port().0 == disconnected_ip
            {
                return Some(ServiceChange::Unregistered { name: name.clone() });
            }
            None
        })
        .collect();

    let has_proxy_clients = current.iter().any(|(_, si)| {
        if let ServiceInfo::Registered(reg) = si {
            reg.clients()
                .keys()
                .any(|c| c.is_proxy() == Some(disconnected_ip))
        } else {
            false
        }
    });
    if has_proxy_clients {
        changes.push(ServiceChange::ProxyDisconnected {
            ip: disconnected_ip,
        });
    }

    changes
}

// --- Teardown helpers (moved from vxlan.rs) ---

async fn teardown_invalidated_service(
    invalidated_service: &str,
    is_failed: bool,
    services: &mut HashMap<String, ServiceInfo>,
    orchestrator: &Orchestrator,
) {
    let services_to_cleanup: Vec<String> = services
        .iter()
        .filter_map(|(service_name, si)| {
            let ServiceInfo::Registered(reg) = si else {
                return None;
            };
            if invalidated_service == service_name
                || reg
                    .dependencies()
                    .contains(&invalidated_service.to_string())
            {
                Some(service_name.clone())
            } else {
                None
            }
        })
        .collect();

    for name in services_to_cleanup {
        teardown_chain(&name, services, orchestrator, None).await;
    }

    if is_failed && let Some(s) = services.get_mut(invalidated_service) {
        s.unregister();
    }
}

async fn teardown_chain(
    name: &str,
    services: &mut HashMap<String, ServiceInfo>,
    orchestrator: &Orchestrator,
    proxy_filter: Option<IpAddr>,
) {
    let Some(ServiceInfo::Registered(reg)) = services.get(name) else {
        return;
    };

    let num_proxy_clients = reg
        .clients()
        .iter()
        .filter(|(c, _)| match proxy_filter {
            Some(ip) => c.is_proxy() == Some(ip),
            None => c.is_proxy().is_some(),
        })
        .count();

    let chain = reg.dependency_chain(name.to_string(), services);

    for ((client_ip, client), (_, server)) in chain {
        let Some(client_ip) = client_ip else { continue };
        if let Some(ServiceInfo::Registered(reg)) = services.get_mut(server.name()) {
            reg.remove_chains(client_ip, &client, num_proxy_clients, orchestrator)
                .await;
        }
    }

    if let Some(ServiceInfo::Registered(reg)) = services.get_mut(name) {
        let service_ip = reg.ip_port().0;
        let proxy_teardowns: Vec<_> = reg
            .clients()
            .iter()
            .filter_map(|(c, ci)| {
                let pip = c.is_proxy()?;
                if proxy_filter.is_none() || proxy_filter == Some(pip) {
                    Some((c.clone(), ci.vxlan_id(), pip))
                } else {
                    None
                }
            })
            .collect();

        for (_, vxlan_id, proxy_ip) in &proxy_teardowns {
            for dest in [service_ip, *proxy_ip] {
                let _ = orchestrator.send_vxlan_teardown(dest, *vxlan_id).await;
            }
        }

        for (client, _, _) in proxy_teardowns {
            reg.clients_mut().remove(&client);
        }
    }
}

// --- Main apply function ---

pub(crate) async fn apply_changes(
    changes: Vec<ServiceChange>,
    services: &mut HashMap<String, ServiceInfo>,
    loaded_services: Option<&HashMap<String, ServiceInfo>>,
    orchestrator: &Orchestrator,
) {
    for change in changes {
        match change {
            ServiceChange::Removed { name } => {
                teardown_invalidated_service(&name, true, services, orchestrator).await;
                services.remove(&name);
            }
            ServiceChange::DepsChanged { name } => {
                teardown_invalidated_service(&name, false, services, orchestrator).await;
            }
            ServiceChange::ReachabilityChanged { name } => {
                teardown_chain(&name, services, orchestrator, None).await;
            }
            ServiceChange::Unregistered { name } => {
                teardown_invalidated_service(&name, true, services, orchestrator).await;
            }
            ServiceChange::ProxyDisconnected { ip } => {
                let proxy_services: Vec<String> = services
                    .iter()
                    .filter(|(_, si)| {
                        if let ServiceInfo::Registered(reg) = si {
                            reg.clients().keys().any(|c| c.is_proxy() == Some(ip))
                        } else {
                            false
                        }
                    })
                    .map(|(name, _)| name.clone())
                    .collect();
                for name in proxy_services {
                    teardown_chain(&name, services, orchestrator, Some(ip)).await;
                }
            }
        }
    }

    // for config updates: update existing services and insert new ones
    if let Some(loaded) = loaded_services {
        for (name, loaded_info) in loaded {
            services
                .entry(name.clone())
                .and_modify(|existing| {
                    existing.update_from_file(loaded_info);
                })
                .or_insert(loaded_info.clone());
        }
    }
}
