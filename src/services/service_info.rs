use crate::orchestrator::Orchestrator;
use crate::proto::nullnet_grpc::Upstream;
use crate::services::clients::{Client, ClientInfo, Clients};
use crate::services::edge::Edge;
use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub(crate) enum ServiceInfo {
    Unregistered(UnregisteredServiceInfo),
    Registered(RegisteredServiceInfo),
}

impl ServiceInfo {
    pub(crate) fn new(dependencies: Vec<String>, is_proxy_reachable: Option<u64>) -> Self {
        ServiceInfo::Unregistered(UnregisteredServiceInfo::new(
            dependencies,
            is_proxy_reachable,
        ))
    }

    pub(crate) fn register(&mut self, ip: IpAddr, port: u16, docker_container: Option<String>) {
        let is_proxy_reachable = self.is_proxy_reachable();
        let dependencies = self.dependencies();
        let clients = self.clients();

        *self = ServiceInfo::Registered(RegisteredServiceInfo {
            dependencies,
            is_proxy_reachable,
            ip,
            port,
            docker_container,
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

    pub(crate) fn is_proxy_reachable(&self) -> Option<u64> {
        match self {
            ServiceInfo::Unregistered(unreg) => unreg.is_proxy_reachable,
            ServiceInfo::Registered(reg) => reg.is_proxy_reachable,
        }
    }

    pub(crate) fn update_from_file(&mut self, loaded: &Self) {
        let loaded_dependencies = loaded.dependencies();
        let loaded_is_proxy_reachable = loaded.is_proxy_reachable();
        match self {
            ServiceInfo::Unregistered(unreg) => {
                unreg.dependencies = loaded_dependencies;
                unreg.is_proxy_reachable = loaded_is_proxy_reachable;
            }
            ServiceInfo::Registered(reg) => {
                reg.dependencies = loaded_dependencies;
                reg.is_proxy_reachable = loaded_is_proxy_reachable;
            }
        }
    }

    pub(crate) fn dependencies(&self) -> Vec<String> {
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
    /// Dependencies of the service.
    dependencies: Vec<String>,
    /// Whether the proxy is reachable for this service, with the associated timeout.
    is_proxy_reachable: Option<u64>,
}

impl UnregisteredServiceInfo {
    fn new(dependencies: Vec<String>, is_proxy_reachable: Option<u64>) -> Self {
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
    /// Whether the proxy is reachable for this service, with the associated timeout.
    is_proxy_reachable: Option<u64>,
    /// IP address of the host.
    ip: IpAddr,
    /// Port of the service.
    port: u16,
    /// Docker container name (if running inside a container).
    docker_container: Option<String>,
    /// Clients connected to this node.
    clients: Clients,
}

impl RegisteredServiceInfo {
    pub(crate) fn dependency_chain(
        &self,
        service_name: String,
        services: &HashMap<String, ServiceInfo>,
    ) -> Vec<Edge> {
        let mut chain = Vec::new();
        let mut current_ip: Option<IpAddr> = Some(self.ip);
        let mut current_name = service_name;
        for dep in &self.dependencies {
            let dep_ip = match services.get(dep) {
                Some(ServiceInfo::Registered(reg)) => Some(reg.ip),
                _ => None,
            };
            let edge = Edge::new(
                current_ip,
                Client::new(current_name.clone(), None),
                dep_ip,
                Client::new(dep.clone(), None),
            );
            chain.push(edge);
            current_ip = dep_ip;
            current_name.clone_from(dep);
        }
        chain
    }

    pub(crate) fn add_chain(&mut self, client: &Client) {
        if let Some(client_info) = self.clients.clients_mut().get_mut(client) {
            client_info.add_active_chain();
        }
    }

    pub(crate) fn set_latest_now(&mut self, client: &Client) {
        if let Some(client_info) = self.clients.clients_mut().get_mut(client) {
            client_info.set_latest_now();
        }
    }

    pub(crate) async fn remove_chains(
        &mut self,
        client_ip: IpAddr,
        client: &Client,
        num_chains: usize,
        orchestrator: &Orchestrator,
    ) {
        let net_to_remove = if let Some(client_info) = self.clients.clients_mut().get_mut(client) {
            client_info.remove_active_chains(num_chains);
            if client_info.active_chains() == 0 {
                Some((
                    client_info.net_id(),
                    client_info.docker_container().cloned(),
                ))
            } else {
                None
            }
        } else {
            None
        };

        if let Some((net_id, client_docker)) = net_to_remove {
            self.clients_mut().remove(client);

            orchestrator
                .send_net_teardown(
                    client_ip,
                    client_docker,
                    self.ip,
                    self.docker_container.clone(),
                    net_id,
                )
                .await;
        }
    }

    pub(crate) fn ip_port(&self) -> (IpAddr, u16) {
        (self.ip, self.port)
    }

    pub(crate) fn docker_container(&self) -> Option<&str> {
        self.docker_container.as_deref()
    }

    pub(crate) fn add_client(&mut self, client: Client, client_info: ClientInfo) {
        self.clients.add_client(client, client_info);
    }

    pub(crate) fn is_client_setup(&self, client: &Client) -> Option<Upstream> {
        self.clients
            .is_client_setup(client)
            .map(|server_net| Upstream {
                ip: server_net.to_string(),
                port: u32::from(self.port),
            })
    }

    pub(crate) fn clients(&self) -> &HashMap<Client, ClientInfo> {
        self.clients.clients()
    }

    pub(crate) fn clients_mut(&mut self) -> &mut HashMap<Client, ClientInfo> {
        self.clients.clients_mut()
    }

    pub(crate) fn dependencies(&self) -> &Vec<String> {
        &self.dependencies
    }

    pub(crate) fn expired_proxy_clients(&self, timeout: Duration) -> Vec<Client> {
        let now = Instant::now();
        self.clients
            .clients()
            .iter()
            .filter(|(c, ci)| c.is_proxy().is_some() && now.duration_since(ci.latest()) >= timeout)
            .map(|(c, _)| c.clone())
            .collect()
    }

    pub(crate) fn nearest_proxy_expiry(&self, timeout: Duration) -> Option<Duration> {
        let now = Instant::now();
        self.clients
            .clients()
            .iter()
            .filter(|(c, _)| c.is_proxy().is_some())
            .map(|(_, ci)| timeout.saturating_sub(now.duration_since(ci.latest())))
            .min()
    }
}
