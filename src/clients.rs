use std::collections::{HashMap, HashSet};
use std::net::IpAddr;

#[derive(Clone, Default)]
pub(crate) struct Clients {
    /// Mapping from browser IP to veth IP.
    proxy_clients: HashMap<IpAddr, IpAddr>,
    service_clients: HashSet<String>,
}

impl Clients {
    pub(crate) fn proxy_clients(&self) -> Vec<IpAddr> {
        self.proxy_clients.keys().copied().collect()
    }

    pub(crate) fn service_clients(&self) -> Vec<String> {
        self.service_clients.iter().cloned().collect()
    }

    pub(crate) fn is_proxy_client_setup(&self, client_ip: IpAddr) -> Option<IpAddr> {
        self.proxy_clients.get(&client_ip).copied()
    }

    pub(crate) fn add_proxy_client(&mut self, client_ip: IpAddr, veth_ip: IpAddr) {
        self.proxy_clients.insert(client_ip, veth_ip);
    }

    pub(crate) fn is_service_client_setup(&self, service: &str) -> bool {
        self.service_clients.contains(service)
    }

    pub(crate) fn add_service_client(&mut self, service: String) {
        self.service_clients.insert(service);
    }
}
