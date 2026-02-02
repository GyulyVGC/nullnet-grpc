use std::collections::HashMap;
use std::net::IpAddr;

#[derive(Clone, Default)]
pub(crate) struct Clients {
    /// Mapping from service client to client info.
    clients: HashMap<String, ClientInfo>,
}

impl Clients {
    pub(crate) fn add_client(&mut self, client: String, client_info: ClientInfo) {
        self.clients.insert(client, client_info);
    }

    pub(crate) fn is_client_setup(&self, client: &str) -> Option<IpAddr> {
        self.clients.get(client).map(|ci| ci.server_veth)
    }

    pub(crate) fn clients(&self) -> &HashMap<String, ClientInfo> {
        &self.clients
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
