use crate::orchestrator::Orchestrator;
use crate::services::service_info::ServiceInfo;
use nullnet_liberror::Error;
use std::collections::HashMap;
use std::net::IpAddr;
pub(crate) async fn cleanup_vxlans_invalidated_service(
    invalidated_service: String,
    is_failed: bool,
    services: &mut HashMap<String, ServiceInfo>,
    orchestrator: &Orchestrator,
) -> Result<(), Error> {
    let mut services_to_cleanup = Vec::new();
    for (service_name, si) in services.iter() {
        let ServiceInfo::Registered(reg) = si else {
            continue;
        };

        if invalidated_service == *service_name || reg.dependencies().contains(&invalidated_service)
        {
            services_to_cleanup.push(service_name.clone());
        }
    }

    for name in services_to_cleanup {
        let _ = cleanup_vxlans_chain(&name, services, orchestrator, None).await;
    }

    // unregister failed service
    if is_failed && let Some(s) = services.get_mut(&invalidated_service) {
        s.unregister();
    }

    Ok(())
}

/// Tears down VXLAN chains for a service's proxy clients and their dependency edges.
/// If `proxy_filter` is `Some`, only chains from that specific proxy IP are cleaned up
/// (e.g. when a proxy node disconnects). If `None`, all proxy chains are cleaned up.
pub(crate) async fn cleanup_vxlans_chain(
    name: &str,
    services: &mut HashMap<String, ServiceInfo>,
    orchestrator: &Orchestrator,
    proxy_filter: Option<IpAddr>,
) -> Result<(), Error> {
    let Some(ServiceInfo::Registered(reg)) = services.get(name) else {
        return Ok(());
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
        if let Some(s) = services.get_mut(server.name())
            && let ServiceInfo::Registered(reg) = s
        {
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

    Ok(())
}
