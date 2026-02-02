use std::collections::HashMap;
use std::net::IpAddr;

#[derive(Clone, Default)]
pub(crate) struct Clients {
    /// Mapping from browser IP to client info.
    proxy_clients: HashMap<IpAddr, ClientInfo>,
    /// Mapping from service name to client info.
    service_clients: HashMap<String, ClientInfo>,
}

impl Clients {
    pub(crate) fn add_service_client(&mut self, service: String, client_info: ClientInfo) {
        self.service_clients.insert(service, client_info);
    }

    pub(crate) fn is_service_client_setup(&self, service: &str) -> bool {
        self.service_clients.contains_key(service)
    }

    pub(crate) fn add_proxy_client(&mut self, client_ip: IpAddr, client_info: ClientInfo) {
        self.proxy_clients.insert(client_ip, client_info);
    }

    pub(crate) fn is_proxy_client_setup(&self, client_ip: IpAddr) -> Option<IpAddr> {
        self.proxy_clients.get(&client_ip).map(|ci| ci.server_veth)
    }

    pub(crate) fn all_clients(&self) -> Vec<(String, ClientInfo)> {
        self.service_clients
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .chain(self.proxy_clients.iter().map(|(k, v)| (k.to_string(), *v)))
            .collect()
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ClientInfo {
    client_veth: IpAddr,
    server_veth: IpAddr,
    vlan_id: u16,
    time_ms: u128,
}

impl ClientInfo {
    pub(crate) fn new(
        client_veth: IpAddr,
        server_veth: IpAddr,
        vlan_id: u16,
        time_ms: u128,
    ) -> Self {
        Self {
            client_veth,
            server_veth,
            vlan_id,
            time_ms,
        }
    }

    pub(crate) fn graphviz_edge_label(&self, show_ends: bool) -> String {
        let Self {
            client_veth,
            server_veth,
            vlan_id,
            time_ms,
        } = self;
        if show_ends {
            format!(
                "[label=\"VLAN {vlan_id} [{time_ms}ms]\", taillabel=\"{client_veth}\", headlabel=\"{server_veth}\"]"
            )
        } else {
            format!("[label=\"VLAN {vlan_id} [{time_ms}ms]\"]")
        }
    }
}
