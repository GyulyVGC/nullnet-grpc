use crate::orchestrator::Orchestrator;
use crate::services::service_info::ServiceInfo;
use nullnet_liberror::Error;
use std::collections::HashMap;

// TODO: cleanup VXLANs (at the moment we're only removing clients from services map)
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

    let num_proxy_clients = reg.clients().iter().filter(|(c, _)| c.is_proxy()).count();

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
        reg.clients_mut().retain(|c, _| !c.is_proxy());
    }

    Ok(())
}
