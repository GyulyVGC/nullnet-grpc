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
    /// All replicas on a specific IP were removed (node disconnected).
    ReplicasRemoved { name: String, ip: IpAddr },
    /// A single replica was removed (host re-registered without this container).
    ReplicaRemoved {
        name: String,
        ip: IpAddr,
        docker_container: Option<String>,
    },
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
                // reachability toggled (Some <-> None)
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
    let mut changes = Vec::new();

    for (name, si) in current {
        let ServiceInfo::Registered(reg) = si else {
            continue;
        };

        for replica in reg.replicas() {
            if replica.ip() != sender_ip {
                continue;
            }
            // Check if this specific replica is still in the sender's list
            let is_in_list = service_list
                .iter()
                .any(|(n, _, dc)| n == name && dc.as_deref() == replica.docker_container());
            if !is_in_list {
                changes.push(ServiceChange::ReplicaRemoved {
                    name: name.clone(),
                    ip: sender_ip,
                    docker_container: replica.docker_container().map(String::from),
                });
            }
        }
    }

    changes
}

pub(crate) fn detect_node_disconnect_changes(
    current: &HashMap<String, ServiceInfo>,
    disconnected_ip: IpAddr,
) -> Vec<ServiceChange> {
    let mut changes: Vec<ServiceChange> = current
        .iter()
        .filter_map(|(name, si)| {
            if let ServiceInfo::Registered(reg) = si
                && reg.has_replica_on_ip(disconnected_ip)
            {
                return Some(ServiceChange::ReplicasRemoved {
                    name: name.clone(),
                    ip: disconnected_ip,
                });
            }
            None
        })
        .collect();

    let has_proxy_clients = current.iter().any(|(_, si)| {
        if let ServiceInfo::Registered(reg) = si {
            reg.replicas().iter().any(|replica| {
                replica
                    .clients()
                    .keys()
                    .any(|c| c.is_proxy() == Some(disconnected_ip))
            })
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

    if is_failed && let Some(si @ ServiceInfo::Registered(_)) = services.get(invalidated_service) {
        let deps = si.dependencies();
        let proxy = si.is_proxy_reachable();
        services.insert(
            invalidated_service.to_string(),
            ServiceInfo::new(deps, proxy),
        );
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

    let all_clients = reg.all_clients_owned();
    let num_proxy_clients = all_clients
        .iter()
        .filter(|(c, _, _, _)| proxy_filter.matches(c))
        .count();

    // Walk the dependency chain to decrement active_chains on each server.
    // The exact replica IPs don't matter here — remove_chains uses the IPs
    // stored in ClientInfo at setup time.
    let deps = reg.dependencies().clone();
    let mut dep_clients: Vec<(String, String)> = Vec::new();
    let mut current_name = name.to_string();
    for dep in &deps {
        dep_clients.push((current_name.clone(), dep.clone()));
        current_name.clone_from(dep);
    }

    for (client_name, server_name) in dep_clients {
        let client = Client::new(client_name, None);
        if let Some(ServiceInfo::Registered(reg)) = services.get_mut(&server_name) {
            reg.remove_chains(&client, num_proxy_clients, orchestrator)
                .await;
        }
    }

    // Tear down proxy edges — client_ip comes from ClientInfo, not reconstructed
    let proxy_teardowns: Vec<_> = all_clients
        .into_iter()
        .filter_map(|(c, ci, replica_ip, replica_docker)| {
            if proxy_filter.matches(&c) && c.is_proxy().is_some() {
                Some((
                    c,
                    ci.client_ip(),
                    ci.net_id(),
                    ci.docker_container().cloned(),
                    replica_ip,
                    replica_docker,
                ))
            } else {
                None
            }
        })
        .collect();

    for (_, client_ip, net_id, client_docker, service_ip, service_docker) in &proxy_teardowns {
        orchestrator
            .send_net_teardown(
                *client_ip,
                client_docker.clone(),
                *service_ip,
                service_docker.clone(),
                *net_id,
            )
            .await;
    }

    if let Some(ServiceInfo::Registered(reg)) = services.get_mut(name) {
        for (client, _, _, _, _, _) in proxy_teardowns {
            reg.remove_client(&client);
        }
    }
}

/// Tear down only the chains that were using replicas at `removed_ip`.
///
/// Checks which service-to-service clients are on the affected replicas
/// and tears down only those services' chains. Chains connected to
/// surviving replicas are left untouched.
///
/// Proxy clients on the affected replicas are torn down directly
/// (not via `teardown_chain`) to avoid tearing down proxy clients on
/// surviving replicas.
async fn teardown_replicas_on_ip(
    name: &str,
    removed_ip: IpAddr,
    services: &mut HashMap<String, ServiceInfo>,
    orchestrator: &Orchestrator,
) {
    let Some(ServiceInfo::Registered(reg)) = services.get(name) else {
        return;
    };

    // Service-to-service clients on the removed replicas — tear down their full chains
    let affected: Vec<String> = reg.service_clients_on_ip(removed_ip);

    // Proxy clients on the removed replicas — tear down directly (not via teardown_chain
    // which would also hit proxy clients on surviving replicas)
    let proxy_teardowns: Vec<_> = reg
        .all_clients_owned()
        .into_iter()
        .filter(|(c, _, rip, _)| c.is_proxy().is_some() && *rip == removed_ip)
        .map(|(c, ci, rip, rd)| {
            (
                c,
                ci.client_ip(),
                ci.net_id(),
                ci.docker_container().cloned(),
                rip,
                rd,
            )
        })
        .collect();

    for sn in affected {
        teardown_chain(&sn, services, orchestrator, ProxyFilter::All).await;
    }

    for (_, client_ip, net_id, client_docker, service_ip, service_docker) in &proxy_teardowns {
        orchestrator
            .send_net_teardown(
                *client_ip,
                client_docker.clone(),
                *service_ip,
                service_docker.clone(),
                *net_id,
            )
            .await;
    }
    if let Some(ServiceInfo::Registered(reg)) = services.get_mut(name) {
        for (client, _, _, _, _, _) in proxy_teardowns {
            reg.remove_client(&client);
        }
    }
}

/// Tear down only the chains that were using a specific replica.
async fn teardown_single_replica(
    name: &str,
    removed_ip: IpAddr,
    removed_docker: Option<&str>,
    services: &mut HashMap<String, ServiceInfo>,
    orchestrator: &Orchestrator,
) {
    let Some(ServiceInfo::Registered(reg)) = services.get(name) else {
        return;
    };

    let affected: Vec<String> = reg.service_clients_on_replica(removed_ip, removed_docker);

    let proxy_teardowns: Vec<_> = reg
        .all_clients_owned()
        .into_iter()
        .filter(|(c, _, rip, rd)| {
            c.is_proxy().is_some() && *rip == removed_ip && rd.as_deref() == removed_docker
        })
        .map(|(c, ci, rip, rd)| {
            (
                c,
                ci.client_ip(),
                ci.net_id(),
                ci.docker_container().cloned(),
                rip,
                rd,
            )
        })
        .collect();

    for sn in affected {
        teardown_chain(&sn, services, orchestrator, ProxyFilter::All).await;
    }

    for (_, client_ip, net_id, client_docker, service_ip, service_docker) in &proxy_teardowns {
        orchestrator
            .send_net_teardown(
                *client_ip,
                client_docker.clone(),
                *service_ip,
                service_docker.clone(),
                *net_id,
            )
            .await;
    }
    if let Some(ServiceInfo::Registered(reg)) = services.get_mut(name) {
        for (client, _, _, _, _, _) in proxy_teardowns {
            reg.remove_client(&client);
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
            ServiceChange::ReplicasRemoved { name, ip } => {
                // Check if removing this IP's replicas would leave the service with zero replicas
                let is_last = if let Some(ServiceInfo::Registered(reg)) = services.get(&name) {
                    reg.replicas().iter().all(|r| r.ip() == ip)
                } else {
                    false
                };

                if is_last {
                    // Last replica gone — full teardown + cascade to dependents
                    teardown_invalidated_service(&name, true, services, orchestrator).await;
                } else {
                    // Partial removal — only tear down chains using replicas on this IP
                    teardown_replicas_on_ip(&name, ip, services, orchestrator).await;
                    if let Some(si) = services.get_mut(&name) {
                        si.remove_replicas_on_ip(ip);
                    }
                }
            }
            ServiceChange::ReplicaRemoved {
                name,
                ip,
                docker_container,
            } => {
                let is_last = if let Some(ServiceInfo::Registered(reg)) = services.get(&name) {
                    reg.replicas().len() == 1
                } else {
                    false
                };

                if is_last {
                    teardown_invalidated_service(&name, true, services, orchestrator).await;
                } else {
                    teardown_single_replica(
                        &name,
                        ip,
                        docker_container.as_deref(),
                        services,
                        orchestrator,
                    )
                    .await;
                    if let Some(si) = services.get_mut(&name) {
                        si.remove_replica(ip, docker_container.as_deref());
                    }
                }
            }
            ServiceChange::ProxyDisconnected { ip } => {
                let proxy_services: Vec<String> = services
                    .iter()
                    .filter(|(_, si)| {
                        if let ServiceInfo::Registered(reg) = si {
                            reg.replicas().iter().any(|replica| {
                                replica.clients().keys().any(|c| c.is_proxy() == Some(ip))
                            })
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
