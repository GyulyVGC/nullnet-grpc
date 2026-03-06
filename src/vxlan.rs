use crate::orchestrator::Orchestrator;
use crate::services::service_info::ServiceInfo;
use nullnet_liberror::Error;
use std::collections::HashMap;

pub(crate) async fn cleanup_vxlans_invalidated_service(
    invalidated_service: String,
    is_failed: bool,
    services: &mut HashMap<String, ServiceInfo>,
    orchestrator: &Orchestrator,
) -> Result<(), Error> {
    let mut services_to_cleanup = Vec::new();
    for (service_name, si) in services.clone() {
        let ServiceInfo::Registered(reg) = si else {
            continue;
        };

        if invalidated_service.eq(&service_name)
            || reg.dependencies().contains(&invalidated_service)
        {
            services_to_cleanup.push(service_name);
        }
    }

    for name in services_to_cleanup {
        let _ = cleanup_vxlans_chain(&name, services, orchestrator).await;
    }

    // unregister failed service
    if is_failed && let Some(s) = services.get_mut(&invalidated_service) {
        s.unregister();
    }

    Ok(())
}

pub(crate) async fn cleanup_vxlans_chain(
    name: &str,
    services: &mut HashMap<String, ServiceInfo>,
    orchestrator: &Orchestrator,
) -> Result<(), Error> {
    let Some(ServiceInfo::Registered(reg)) = services.get(name) else {
        return Ok(());
    };

    let num_proxy_clients = reg.clients().iter().filter(|(c, _)| c.is_proxy().is_some()).count();

    let chain = reg.dependency_chain(name.to_string(), services)?;

    for ((client_ip, client), (_, server)) in chain {
        if let Some(s) = services.get_mut(&server.name())
            && let ServiceInfo::Registered(reg) = s
        {
            reg.remove_chains(client_ip, &client, num_proxy_clients, orchestrator)
                .await;
        }
    }

    if let Some(ServiceInfo::Registered(reg)) = services.get_mut(name) {
        // TODO: remove these clones where we iter and also need mutable access inside the loop
        for (c, ci) in reg.clients().clone() {
            if let Some(proxy_ip) = c.is_proxy() {
                for dest in [reg.ip_port().0, proxy_ip] {
                    let _ = orchestrator.send_vxlan_teardown(dest, ci.vxlan_id()).await;
                }
                reg.clients_mut().remove(&c);
            }
        }
    }

    Ok(())
}
