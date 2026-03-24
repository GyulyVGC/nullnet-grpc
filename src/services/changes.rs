use crate::orchestrator::Orchestrator;
use crate::services::clients::Client;
use crate::services::service_info::ServiceInfo;
use std::collections::HashMap;
use std::net::IpAddr;
use std::time::Duration;

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
    /// A proxy client's timeout expired; tear down its chains.
    ProxyClientTimedOut { name: String, client: Client },
}

enum ProxyFilter<'a> {
    All,
    ByIp(IpAddr),
    ByClient(&'a Client),
}

impl ProxyFilter<'_> {
    fn matches(&self, client: &Client) -> bool {
        match self {
            ProxyFilter::All => client.is_proxy().is_some(),
            ProxyFilter::ByIp(ip) => client.is_proxy() == Some(*ip),
            ProxyFilter::ByClient(c) => client == *c,
        }
    }
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

    // services with changed deps, reachability, or timeout
    for (name, loaded_info) in loaded {
        if let Some(old_info) = current.get(name) {
            if loaded_info.dependencies() != old_info.dependencies() {
                changes.push(ServiceChange::DepsChanged { name: name.clone() });
            } else if loaded_info.is_proxy_reachable().is_some()
                != old_info.is_proxy_reachable().is_some()
            {
                // reachability toggled (Some ↔ None)
                changes.push(ServiceChange::ReachabilityChanged { name: name.clone() });
            } else if let (Some(new_timeout), Some(old_timeout)) = (
                loaded_info.is_proxy_reachable(),
                old_info.is_proxy_reachable(),
            ) && new_timeout != old_timeout
                && new_timeout > 0
                && (old_timeout == 0 || new_timeout < old_timeout)
            {
                // timeout tightened or introduced: expire clients already past the new limit
                if let ServiceInfo::Registered(reg) = old_info {
                    for client in reg.expired_proxy_clients(Duration::from_secs(new_timeout)) {
                        changes.push(ServiceChange::ProxyClientTimedOut {
                            name: name.clone(),
                            client,
                        });
                    }
                }
            }
        }
    }

    changes
}

pub(crate) fn detect_services_list_changes(
    current: &HashMap<String, ServiceInfo>,
    sender_ip: IpAddr,
    service_list: &[(String, u16, Option<String>)],
) -> Vec<ServiceChange> {
    current
        .iter()
        .filter_map(|(name, si)| {
            if let ServiceInfo::Registered(reg) = si
                && reg.ip_port().0 == sender_ip
                && !service_list.iter().any(|(s, _, _)| s == name)
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

// --- Teardown helpers ---

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
        teardown_chain(&name, services, orchestrator, ProxyFilter::All).await;
    }

    if is_failed && let Some(s) = services.get_mut(invalidated_service) {
        s.unregister();
    }
}

async fn teardown_chain(
    name: &str,
    services: &mut HashMap<String, ServiceInfo>,
    orchestrator: &Orchestrator,
    proxy_filter: ProxyFilter<'_>,
) {
    let Some(ServiceInfo::Registered(reg)) = services.get(name) else {
        return;
    };

    let num_proxy_clients = reg
        .clients()
        .iter()
        .filter(|(c, _)| proxy_filter.matches(c))
        .count();

    let chain = reg.dependency_chain(name.to_string(), services);

    for edge in chain {
        let (client_ip, client) = edge.client;
        let (_, server) = edge.server;

        let Some(client_ip) = client_ip else { continue };
        if let Some(ServiceInfo::Registered(reg)) = services.get_mut(server.name()) {
            reg.remove_chains(client_ip, &client, num_proxy_clients, orchestrator)
                .await;
        }
    }

    if let Some(ServiceInfo::Registered(reg)) = services.get_mut(name) {
        let service_ip = reg.ip_port().0;
        let service_docker = reg.docker_container().map(String::from);
        let proxy_teardowns: Vec<_> = reg
            .clients()
            .iter()
            .filter_map(|(c, ci)| {
                let pip = c.is_proxy()?;
                if proxy_filter.matches(c) {
                    Some((c.clone(), ci.net_id(), pip, ci.docker_container().cloned()))
                } else {
                    None
                }
            })
            .collect();

        for (_, net_id, proxy_ip, client_docker) in &proxy_teardowns {
            orchestrator
                .send_net_teardown(
                    *proxy_ip,
                    client_docker.clone(),
                    service_ip,
                    service_docker.clone(),
                    *net_id,
                )
                .await;
        }

        for (client, _, _, _) in proxy_teardowns {
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
                teardown_chain(&name, services, orchestrator, ProxyFilter::All).await;
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
                    teardown_chain(&name, services, orchestrator, ProxyFilter::ByIp(ip)).await;
                }
            }
            ServiceChange::ProxyClientTimedOut { name, client } => {
                println!(
                    "Proxy client '{}' timed out on service '{name}'",
                    client.display_name()
                );
                teardown_chain(
                    &name,
                    services,
                    orchestrator,
                    ProxyFilter::ByClient(&client),
                )
                .await;
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
