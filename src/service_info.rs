use crate::clients::Clients;
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
    pub fn new(dependencies: Vec<String>, is_proxy_reachable: bool) -> Self {
        ServiceInfo::Unregistered(UnregisteredServiceInfo::new(
            dependencies,
            is_proxy_reachable,
        ))
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

    pub(crate) fn graphviz_style(&self) -> &'static str {
        match self {
            ServiceInfo::Unregistered(unreg) if unreg.is_proxy_reachable => {
                "[style=solid, color=red]"
            }
            ServiceInfo::Unregistered(_) => "[style=dashed, color=red]",
            ServiceInfo::Registered(reg) if reg.is_proxy_reachable => "[style=solid, color=green]",
            ServiceInfo::Registered(_) => "[style=dashed, color=green]",
        }
    }
}

#[derive(Clone)]
pub(crate) struct UnregisteredServiceInfo {
    dependencies: Vec<String>,
    is_proxy_reachable: bool,
}

impl UnregisteredServiceInfo {
    fn new(dependencies: Vec<String>, is_proxy_reachable: bool) -> Self {
        Self {
            dependencies,
            is_proxy_reachable,
        }
    }

    fn register(self, ip: IpAddr, port: u16) -> RegisteredServiceInfo {
        RegisteredServiceInfo {
            ip,
            port,
            is_proxy_reachable: self.is_proxy_reachable,
            dependencies: self.dependencies,
            clients: Clients::default(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct RegisteredServiceInfo {
    /// Dependencies of the service.
    dependencies: Vec<String>,
    is_proxy_reachable: bool,
    /// IP address of the host.
    ip: IpAddr,
    /// Port of the service.
    port: u16,
    /// Clients connected to this node.
    clients: Clients,
}

impl RegisteredServiceInfo {
    pub(crate) async fn dependency_chain(
        &self,
        service_name: String,
        services: &Arc<RwLock<HashMap<String, ServiceInfo>>>,
    ) -> Result<Vec<((IpAddr, String), (IpAddr, String))>, Error> {
        let mut chain = Vec::new();
        let mut current_ip = self.ip;
        let mut current_name = service_name;
        for dep in &self.dependencies {
            let ServiceInfo::Registered(dep_reg) = services
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
            chain.push(((current_ip, current_name.clone()), (dep_ip, dep.clone())));
            current_ip = dep_ip;
            current_name = dep.clone();
        }

        Ok(chain)
    }

    fn re_register(self, ip: IpAddr, port: u16) -> Self {
        Self {
            dependencies: self.dependencies,
            is_proxy_reachable: self.is_proxy_reachable,
            ip,
            port,
            clients: self.clients,
        }
    }

    pub(crate) fn ip_port(&self) -> (IpAddr, u16) {
        (self.ip, self.port)
    }

    pub(crate) fn is_proxy_client_setup(&self, client_ip: IpAddr) -> Option<Upstream> {
        self.clients
            .is_proxy_client_setup(client_ip)
            .map(|veth_ip| Upstream {
                ip: veth_ip.to_string(),
                port: u32::from(self.port),
            })
    }

    pub(crate) fn is_service_client_setup(&self, service_name: &str) -> bool {
        self.clients.is_service_client_setup(service_name)
    }

    pub(crate) fn is_proxy_reachable(&self) -> bool {
        self.is_proxy_reachable
    }

    pub(crate) fn add_proxy_client(&mut self, client_ip: IpAddr, veth_ip: IpAddr) {
        self.clients.add_proxy_client(client_ip, veth_ip);
    }

    pub(crate) fn add_service_client(&mut self, service: String) {
        self.clients.add_service_client(service);
    }

    pub(crate) fn proxy_clients(&self) -> Vec<IpAddr> {
        self.clients.proxy_clients()
    }

    pub(crate) fn service_clients(&self) -> Vec<String> {
        self.clients.service_clients()
    }
}

#[derive(Deserialize, Debug)]
pub(crate) struct ServicesToml {
    services: Vec<ServiceToml>,
}

impl ServicesToml {
    pub fn services_map(&self) -> HashMap<String, ServiceInfo> {
        let mut ret_val: HashMap<String, ServiceInfo> = HashMap::new();

        // first insert proxy-reachable services
        for s in &self.services {
            ret_val.insert(
                s.name.clone(),
                ServiceInfo::new(s.dependencies.clone(), true),
            );
        }

        for s in &self.services {
            for d in &s.dependencies {
                if !ret_val.contains_key(d) {
                    ret_val.insert(d.clone(), ServiceInfo::new(Vec::new(), false));
                }
            }
        }

        ret_val
    }
}

#[derive(Deserialize, Debug)]
pub(crate) struct ServiceToml {
    name: String,
    dependencies: Vec<String>,
}
