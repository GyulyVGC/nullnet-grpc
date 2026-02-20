use crate::services::service_info::ServiceInfo;
use nullnet_liberror::Error;
use std::collections::HashMap;

// TODO: cleanup VLANs (at the moment we're only removing clients from services map)
pub(crate) async fn cleanup_vlans_failed_service(
    failed_service: String,
    services: &mut HashMap<String, ServiceInfo>,
) -> Result<(), Error> {
    let mut services_to_cleanup = Vec::new();
    for (service_name, si) in services.clone() {
        let ServiceInfo::Registered(reg) = si else {
            continue;
        };

        if failed_service.eq(&service_name) || reg.dependencies().contains(&failed_service) {
            services_to_cleanup.push(service_name);
        }
    }

    for name in services_to_cleanup {
        cleanup_vlans_chain(name.clone(), services)?;
    }

    // unregister failed service
    if let Some(s) = services.get_mut(&failed_service) {
        s.unregister();
    }

    Ok(())
}

pub(crate) fn cleanup_vlans_chain(
    name: String,
    services: &mut HashMap<String, ServiceInfo>,
) -> Result<(), Error> {
    let Some(ServiceInfo::Registered(reg)) = services.get(&name) else {
        return Ok(());
    };

    let num_proxy_clients = reg.clients().iter().filter(|(c, _)| c.is_proxy()).count();

    let chain = reg.dependency_chain(name.clone(), services)?;

    for ((_, client), (_, server)) in chain {
        if let Some(s) = services.get_mut(&server.name())
            && let ServiceInfo::Registered(reg) = s
        {
            reg.remove_chains(&client, num_proxy_clients);
            if reg.chains(&client) == 0 {
                reg.clients_mut().remove(&client);
            }
        }
    }

    if let Some(ServiceInfo::Registered(reg)) = services.get_mut(&name) {
        reg.clients_mut().retain(|c, _| !c.is_proxy());
    }

    Ok(())
}
