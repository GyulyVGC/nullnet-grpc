use std::collections::HashMap;
use std::net::IpAddr;

#[derive(Clone, Default, Debug)]
pub(crate) struct Clients {
    /// Mapping from service client to client info.
    clients: HashMap<Client, ClientInfo>,
}

impl Clients {
    pub(crate) fn add_client(&mut self, client: Client, client_info: ClientInfo) {
        self.clients.insert(client, client_info);
    }

    pub(crate) fn is_client_setup(&self, client: &Client) -> Option<IpAddr> {
        self.clients.get(client).map(|ci| ci.server_veth)
    }

    pub(crate) fn clients(&self) -> &HashMap<Client, ClientInfo> {
        &self.clients
    }
}

#[derive(Clone, Eq, PartialEq, Hash, Debug)]
pub(crate) struct Client {
    name: String,
    proxy: Option<IpAddr>,
}

impl Client {
    pub(crate) fn new(name: String, proxy: Option<IpAddr>) -> Self {
        Self { name, proxy }
    }

    pub(crate) fn name(&self) -> String {
        if let Some(proxy) = self.proxy {
            format!("{} (via {})", self.name, proxy)
        } else {
            self.name.clone()
        }
    }

    pub(crate) fn is_proxy(&self) -> bool {
        self.proxy.is_some()
    }
}

#[derive(Clone, Copy, Debug)]
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
