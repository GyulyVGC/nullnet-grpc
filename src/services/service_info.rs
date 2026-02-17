use crate::proto::nullnet_grpc::Upstream;
use crate::services::clients::{Client, ClientInfo, Clients};
use nullnet_liberror::{Error, ErrorHandler, Location, location};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Clone, Debug)]
pub(crate) enum ServiceInfo {
    Unregistered(UnregisteredServiceInfo),
    Registered(RegisteredServiceInfo),
}

impl ServiceInfo {
    pub(crate) fn new(dependencies: Vec<String>, is_proxy_reachable: bool) -> Self {
        ServiceInfo::Unregistered(UnregisteredServiceInfo::new(
            dependencies,
            is_proxy_reachable,
        ))
    }

    pub(crate) fn register(&mut self, ip: IpAddr, port: u16) {
        let is_proxy_reachable = self.is_proxy_reachable();
        let dependencies = self.dependencies();
        let clients = self.clients();

        *self = ServiceInfo::Registered(RegisteredServiceInfo {
            dependencies,
            is_proxy_reachable,
            ip,
            port,
            clients,
        });
    }

    pub(crate) fn unregister(&mut self) {
        if let ServiceInfo::Registered(reg) = self {
            *self = ServiceInfo::Unregistered(UnregisteredServiceInfo::new(
                reg.dependencies.clone(),
                reg.is_proxy_reachable,
            ));
        }
    }

    pub(crate) fn is_proxy_reachable(&self) -> bool {
        match self {
            ServiceInfo::Unregistered(unreg) => unreg.is_proxy_reachable,
            ServiceInfo::Registered(reg) => reg.is_proxy_reachable,
        }
    }

    fn dependencies(&self) -> Vec<String> {
        match self {
            ServiceInfo::Unregistered(unreg) => unreg.dependencies.clone(),
            ServiceInfo::Registered(reg) => reg.dependencies.clone(),
        }
    }

    fn clients(&self) -> Clients {
        match self {
            ServiceInfo::Unregistered(_) => Clients::default(),
            ServiceInfo::Registered(reg) => reg.clients.clone(),
        }
    }
}

#[derive(Clone, Debug)]
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
}

#[derive(Clone, Debug)]
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
    ) -> Result<Vec<((IpAddr, Client), (IpAddr, Client))>, Error> {
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
            chain.push((
                (current_ip, Client::new(current_name.clone(), None)),
                (dep_ip, Client::new(dep.clone(), None)),
            ));
            current_ip = dep_ip;
            current_name = dep.clone();
        }

        Ok(chain)
    }

    pub(crate) fn ip_port(&self) -> (IpAddr, u16) {
        (self.ip, self.port)
    }

    pub(crate) fn add_client(&mut self, client: Client, client_info: ClientInfo) {
        self.clients.add_client(client, client_info);
    }

    pub(crate) fn is_client_setup(&self, client: &Client) -> Option<Upstream> {
        self.clients
            .is_client_setup(client)
            .map(|veth_ip| Upstream {
                ip: veth_ip.to_string(),
                port: u32::from(self.port),
            })
    }

    pub(crate) fn clients(&self) -> &HashMap<Client, ClientInfo> {
        self.clients.clients()
    }
}
