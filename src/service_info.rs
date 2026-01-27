use crate::proto::nullnet_grpc::Upstream;
use nullnet_liberror::{Error, ErrorHandler, Location, location};
use serde::Deserialize;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Clone)]
pub(crate) enum ServiceInfo {
    Unregistered(UnregisteredServiceInfo),
    Registered(RegisteredServiceInfo),
}

impl ServiceInfo {
    pub fn new(dependencies: Vec<String>) -> Self {
        ServiceInfo::Unregistered(UnregisteredServiceInfo::new(dependencies))
    }

    pub fn register(self, ip: IpAddr, port: u16) -> ServiceInfo {
        match self {
            ServiceInfo::Unregistered(unreg) => ServiceInfo::Registered(unreg.register(ip, port)),
            ServiceInfo::Registered(reg) => Self::Registered(reg.re_register(ip, port)),
        }
    }

    pub fn as_registered(&self) -> Option<&RegisteredServiceInfo> {
        match self {
            ServiceInfo::Unregistered(_) => None,
            ServiceInfo::Registered(reg) => Some(reg),
        }
    }
}

#[derive(Clone)]
pub(crate) struct UnregisteredServiceInfo {
    dependencies: Vec<String>,
}

impl UnregisteredServiceInfo {
    fn new(dependencies: Vec<String>) -> Self {
        Self { dependencies }
    }

    fn register(self, ip: IpAddr, port: u16) -> RegisteredServiceInfo {
        RegisteredServiceInfo {
            ip,
            port,
            dependencies: self.dependencies,
            clients: HashMap::new(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct RegisteredServiceInfo {
    /// IP address of the host.
    ip: IpAddr,
    /// Port of the service.
    port: u16,
    /// Dependencies of the service.
    dependencies: Vec<String>,
    /// Mapping from browser IP to veth IP.
    clients: HashMap<IpAddr, IpAddr>,
}

impl RegisteredServiceInfo {
    pub(crate) async fn dependency_chain(
        &self,
        dependencies: &Arc<RwLock<HashMap<String, DependencyInfo>>>,
    ) -> Result<Vec<(IpAddr, (IpAddr, String))>, Error> {
        let mut chain = Vec::new();
        let mut current_ip = self.ip;
        for dep in &self.dependencies {
            let DependencyInfo::Registered(dep_reg) = dependencies
                .read()
                .await
                .get(dep)
                .cloned()
                .ok_or("Dependency service not found")
                .handle_err(location!())?
            else {
                return Err("Dependency service is not registered yet").handle_err(location!());
            };
            let dep_ip = dep_reg.ip;
            chain.push((current_ip, (dep_ip, dep.clone())));
            current_ip = dep_ip;
        }

        Ok(chain)
    }

    fn re_register(self, ip: IpAddr, port: u16) -> Self {
        Self {
            ip,
            port,
            dependencies: self.dependencies,
            clients: self.clients,
        }
    }

    pub(crate) fn ip_port(&self) -> (IpAddr, u16) {
        (self.ip, self.port)
    }

    pub(crate) fn is_client_setup(&self, client_ip: IpAddr) -> Option<Upstream> {
        self.clients.get(&client_ip).map(|veth_ip| Upstream {
            ip: veth_ip.to_string(),
            port: u32::from(self.port),
        })
    }

    pub(crate) fn are_dependencies_setup(&self) -> bool {
        !self.clients.is_empty()
    }

    pub(crate) fn setup_client(&mut self, client_ip: IpAddr, veth_ip: IpAddr) {
        self.clients.insert(client_ip, veth_ip);
    }
}

#[derive(Clone)]
pub(crate) enum DependencyInfo {
    Unregistered,
    Registered(RegisteredDependencyInfo),
}

impl DependencyInfo {
    pub fn registered(ip: IpAddr) -> DependencyInfo {
        DependencyInfo::Registered(RegisteredDependencyInfo { ip })
    }
}

#[derive(Clone)]
pub(crate) struct RegisteredDependencyInfo {
    /// IP address of the host.
    ip: IpAddr,
    // Port of the service. TODO: ports are not used at the moment for dependencies
    // port: u16,
}

#[derive(Deserialize, Debug)]
pub(crate) struct ServicesToml {
    services: Vec<ServiceToml>,
}

impl ServicesToml {
    pub fn dependencies_map(&self) -> HashMap<String, DependencyInfo> {
        self.services
            .iter()
            .flat_map(|service| {
                service
                    .dependencies
                    .iter()
                    .map(|dep| (dep.clone(), DependencyInfo::Unregistered))
            })
            .collect()
    }

    pub fn services_map(&self) -> HashMap<String, ServiceInfo> {
        self.services
            .iter()
            .map(|service| {
                (
                    service.name.clone(),
                    ServiceInfo::new(service.dependencies.clone()),
                )
            })
            .collect()
    }
}

#[derive(Deserialize, Debug)]
pub(crate) struct ServiceToml {
    name: String,
    dependencies: Vec<String>,
}
