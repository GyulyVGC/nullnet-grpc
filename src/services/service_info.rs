use crate::orchestrator::Orchestrator;
use crate::proto::nullnet_grpc::Upstream;
use crate::services::clients::{Client, ClientInfo, Clients};
use crate::services::edge::Edge;
use std::collections::{HashMap, HashSet};
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

    pub(crate) fn add_replica(&mut self, ip: IpAddr, port: u16, docker_container: Option<String>) {
        match self {
            ServiceInfo::Unregistered(unreg) => {
                *self = ServiceInfo::Registered(RegisteredServiceInfo {
                    dependencies: unreg.dependencies.clone(),
                    is_proxy_reachable: unreg.is_proxy_reachable,
                    replicas: vec![Replica::new(ip, port, docker_container)],
                });
            }
            ServiceInfo::Registered(reg) => {
                if let Some(replica) = reg
                    .replicas
                    .iter_mut()
                    .find(|r| r.matches_identity(ip, docker_container.as_deref()))
                {
                    replica.port = port;
                } else {
                    reg.replicas.push(Replica::new(ip, port, docker_container));
                }
            }
        }
    }

    /// Remove all replicas on the given IP.
    /// Transitions to `Unregistered` if no replicas remain.
    pub(crate) fn remove_replicas_on_ip(&mut self, ip: IpAddr) {
        if let ServiceInfo::Registered(reg) = self {
            reg.replicas.retain(|r| r.ip != ip);
            if reg.replicas.is_empty() {
                *self = ServiceInfo::Unregistered(UnregisteredServiceInfo::new(
                    reg.dependencies.clone(),
                    reg.is_proxy_reachable,
                ));
            }
        }
    }

    /// Remove a single replica identified by `(ip, docker_container)`.
    /// Transitions to `Unregistered` if no replicas remain.
    pub(crate) fn remove_replica(&mut self, ip: IpAddr, docker_container: Option<&str>) {
        if let ServiceInfo::Registered(reg) = self {
            reg.replicas
                .retain(|r| !r.matches_identity(ip, docker_container));
            if reg.replicas.is_empty() {
                *self = ServiceInfo::Unregistered(UnregisteredServiceInfo::new(
                    reg.dependencies.clone(),
                    reg.is_proxy_reachable,
                ));
            }
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
pub(crate) struct Replica {
    ip: IpAddr,
    port: u16,
    docker_container: Option<String>,
    clients: Clients,
}

impl Replica {
    fn new(ip: IpAddr, port: u16, docker_container: Option<String>) -> Self {
        Self {
            ip,
            port,
            docker_container,
            clients: Clients::default(),
        }
    }

    pub(crate) fn ip(&self) -> IpAddr {
        self.ip
    }

    pub(crate) fn port(&self) -> u16 {
        self.port
    }

    pub(crate) fn docker_container(&self) -> Option<&str> {
        self.docker_container.as_deref()
    }

    pub(crate) fn clients(&self) -> &HashMap<Client, ClientInfo> {
        self.clients.clients()
    }

    /// A replica is uniquely identified by its `(ip, docker_container)` pair.
    pub(crate) fn matches_identity(&self, ip: IpAddr, docker_container: Option<&str>) -> bool {
        self.ip == ip && self.docker_container.as_deref() == docker_container
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RegisteredServiceInfo {
    /// Dependencies of the service.
    dependencies: Vec<String>,
    /// Whether the proxy is reachable for this service, with the associated timeout.
    is_proxy_reachable: Option<u64>,
    /// Replicas of this service.
    replicas: Vec<Replica>,
}

impl RegisteredServiceInfo {
    pub(crate) fn dependency_chain(
        &self,
        service_name: String,
        service_ip: IpAddr,
        service_docker: Option<&str>,
        services: &HashMap<String, ServiceInfo>,
    ) -> Vec<Edge> {
        let mut chain = Vec::new();
        let mut current_ip: Option<IpAddr> = Some(service_ip);
        let mut current_docker: Option<String> = service_docker.map(String::from);
        let mut current_name = service_name;
        for dep in &self.dependencies {
            let (dep_ip, dep_docker) = match services.get(dep) {
                Some(ServiceInfo::Registered(reg)) => {
                    let r = reg.pick_replica_least_clients();
                    (Some(r.ip), r.docker_container.clone())
                }
                _ => (None, None),
            };
            let client = match current_ip {
                Some(ip) => Client::new_service(current_name.clone(), ip, current_docker.clone()),
                None => Client::new(current_name.clone(), None),
            };
            let edge = Edge::new(
                current_ip,
                client,
                current_docker,
                dep_ip,
                Client::new(dep.clone(), None),
                dep_docker.clone(),
            );
            chain.push(edge);
            current_ip = dep_ip;
            current_docker = dep_docker;
            current_name.clone_from(dep);
        }
        chain
    }

    /// Invariant: a given `Client` exists on exactly one replica (sticky sessions).
    /// These methods search across replicas and update the first (only) match.
    pub(crate) fn add_chain(&mut self, client: &Client) {
        for replica in &mut self.replicas {
            if let Some(client_info) = replica.clients.clients_mut().get_mut(client) {
                client_info.add_active_chain();
                return;
            }
        }
    }

    pub(crate) fn set_latest_now(&mut self, client: &Client) {
        for replica in &mut self.replicas {
            if let Some(client_info) = replica.clients.clients_mut().get_mut(client) {
                client_info.set_latest_now();
                return;
            }
        }
    }

    /// Decrement `active_chains` for a specific client entry.
    /// If it reaches 0, the VXLAN is torn down and the entry is removed.
    pub(crate) async fn decrement_chain(
        &mut self,
        client: &Client,
        orchestrator: &Orchestrator,
    ) {
        for replica in &mut self.replicas {
            if let Some(ci) = replica.clients.clients_mut().get_mut(client) {
                ci.remove_active_chains(1);
                if ci.active_chains() == 0 {
                    let ci = replica.clients.clients_mut().remove(client).unwrap();
                    orchestrator
                        .send_net_teardown(
                            ci.client_ip(),
                            ci.docker_container().cloned(),
                            replica.ip,
                            replica.docker_container.clone(),
                            ci.net_id(),
                        )
                        .await;
                }
                return;
            }
        }
    }

    /// Find which server replica hosts a given client entry.
    /// Returns the server replica's `(ip, docker_container)`.
    pub(crate) fn client_replica(&self, client: &Client) -> Option<(IpAddr, Option<String>)> {
        self.replicas
            .iter()
            .find(|r| r.clients.clients().contains_key(client))
            .map(|r| (r.ip, r.docker_container.clone()))
    }

    /// Select the replica with the fewest active clients.
    pub(crate) fn pick_replica_least_clients(&self) -> &Replica {
        self.replicas
            .iter()
            .min_by_key(|r| r.clients.clients().len())
            .expect("registered service has no replicas")
    }

    pub(crate) fn add_client_to_replica(
        &mut self,
        replica_ip: IpAddr,
        replica_docker: Option<&str>,
        client: Client,
        client_info: ClientInfo,
    ) {
        if let Some(replica) = self
            .replicas
            .iter_mut()
            .find(|r| r.matches_identity(replica_ip, replica_docker))
        {
            replica.clients.add_client(client, client_info);
        }
    }

    pub(crate) fn is_client_setup(&self, client: &Client) -> Option<Upstream> {
        for replica in &self.replicas {
            if let Some(server_net) = replica.clients.is_client_setup(client) {
                return Some(Upstream {
                    ip: server_net.to_string(),
                    port: u32::from(replica.port),
                });
            }
        }
        None
    }

    /// Check if a specific replica already has this client.
    pub(crate) fn is_client_on_replica(
        &self,
        client: &Client,
        ip: IpAddr,
        docker: Option<&str>,
    ) -> bool {
        self.replicas
            .iter()
            .filter(|r| r.matches_identity(ip, docker))
            .any(|r| r.clients.is_client_setup(client).is_some())
    }

    pub(crate) fn remove_client(&mut self, client: &Client) {
        for replica in &mut self.replicas {
            if replica.clients.clients_mut().remove(client).is_some() {
                return;
            }
        }
    }

    pub(crate) fn replicas(&self) -> &[Replica] {
        &self.replicas
    }

    pub(crate) fn dependencies(&self) -> &Vec<String> {
        &self.dependencies
    }

    pub(crate) fn expired_proxy_clients(&self, timeout: Duration) -> Vec<Client> {
        let now = Instant::now();
        self.replicas
            .iter()
            .flat_map(|replica| {
                replica
                    .clients
                    .clients()
                    .iter()
                    .filter(|(c, ci)| {
                        c.is_proxy().is_some() && now.duration_since(ci.latest()) >= timeout
                    })
                    .map(|(c, _)| c.clone())
            })
            .collect()
    }

    pub(crate) fn nearest_proxy_expiry(&self, timeout: Duration) -> Option<Duration> {
        let now = Instant::now();
        self.replicas
            .iter()
            .flat_map(|replica| {
                replica
                    .clients
                    .clients()
                    .iter()
                    .filter(|(c, _)| c.is_proxy().is_some())
                    .map(|(_, ci)| timeout.saturating_sub(now.duration_since(ci.latest())))
            })
            .min()
    }

    /// Return unique service names (non-proxy clients) connected to replicas at the given IP.
    pub(crate) fn service_clients_on_ip(&self, ip: IpAddr) -> Vec<String> {
        let names: HashSet<String> = self
            .replicas
            .iter()
            .filter(|r| r.ip == ip)
            .flat_map(|r| r.clients.clients().keys())
            .filter(|c| c.is_proxy().is_none())
            .map(|c| c.name().to_string())
            .collect();
        names.into_iter().collect()
    }

    /// Return unique service names (non-proxy clients) connected to a specific replica.
    pub(crate) fn service_clients_on_replica(
        &self,
        ip: IpAddr,
        docker_container: Option<&str>,
    ) -> Vec<String> {
        let names: HashSet<String> = self
            .replicas
            .iter()
            .filter(|r| r.matches_identity(ip, docker_container))
            .flat_map(|r| r.clients.clients().keys())
            .filter(|c| c.is_proxy().is_none())
            .map(|c| c.name().to_string())
            .collect();
        names.into_iter().collect()
    }

    pub(crate) fn has_replica_on_ip(&self, ip: IpAddr) -> bool {
        self.replicas.iter().any(|r| r.ip == ip)
    }

    #[cfg(test)]
    pub(crate) fn client_count(&self) -> usize {
        self.replicas
            .iter()
            .map(|r| r.clients.clients().len())
            .sum()
    }

    #[cfg(test)]
    pub(crate) fn has_clients(&self) -> bool {
        self.replicas
            .iter()
            .any(|r| !r.clients.clients().is_empty())
    }

    /// Collect all clients across all replicas as owned data (for teardown iteration).
    pub(crate) fn all_clients_owned(&self) -> Vec<(Client, ClientInfo, IpAddr, Option<String>)> {
        self.replicas
            .iter()
            .flat_map(|replica| {
                replica.clients.clients().iter().map(move |(c, ci)| {
                    (
                        c.clone(),
                        ci.clone(),
                        replica.ip,
                        replica.docker_container.clone(),
                    )
                })
            })
            .collect()
    }
}
