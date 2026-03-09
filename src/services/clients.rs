use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};

#[derive(Clone, Default, Debug)]
pub(super) struct Clients {
    /// Mapping from service client to client info.
    clients: HashMap<Client, ClientInfo>,
}

impl Clients {
    pub(super) fn add_client(&mut self, client: Client, client_info: ClientInfo) {
        self.clients.insert(client, client_info);
    }

    pub(super) fn is_client_setup(&self, client: &Client) -> Option<Ipv4Addr> {
        self.clients.get(client).map(|ci| ci.server_br)
    }

    pub(super) fn clients(&self) -> &HashMap<Client, ClientInfo> {
        &self.clients
    }

    pub(super) fn clients_mut(&mut self) -> &mut HashMap<Client, ClientInfo> {
        &mut self.clients
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

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn display_name(&self) -> String {
        if let Some(proxy) = self.proxy {
            format!("{} (via {})", self.name, proxy)
        } else {
            self.name.clone()
        }
    }

    pub(crate) fn is_proxy(&self) -> Option<IpAddr> {
        self.proxy
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ClientInfo {
    client_br: Ipv4Addr,
    server_br: Ipv4Addr,
    vxlan_id: u32,
    time_ms: u128,
    active_chains: usize,
}

impl ClientInfo {
    pub(crate) fn new(
        client_br: Ipv4Addr,
        server_br: Ipv4Addr,
        vxlan_id: u32,
        time_ms: u128,
    ) -> Self {
        Self {
            client_br,
            server_br,
            vxlan_id,
            time_ms,
            active_chains: 0,
        }
    }

    pub(crate) fn placeholder() -> Self {
        Self {
            client_br: Ipv4Addr::UNSPECIFIED,
            server_br: Ipv4Addr::UNSPECIFIED,
            vxlan_id: 0,
            time_ms: 0,
            active_chains: 0,
        }
    }

    pub(crate) fn client_br(&self) -> Ipv4Addr {
        self.client_br
    }

    pub(crate) fn server_br(&self) -> Ipv4Addr {
        self.server_br
    }

    pub(crate) fn vxlan_id(&self) -> u32 {
        self.vxlan_id
    }

    pub(crate) fn time_ms(&self) -> u128 {
        self.time_ms
    }

    pub(super) fn add_active_chain(&mut self) {
        self.active_chains += 1;
    }

    pub(super) fn remove_active_chains(&mut self, num_chains: usize) {
        self.active_chains = self.active_chains.saturating_sub(num_chains);
    }

    pub(super) fn active_chains(&self) -> usize {
        self.active_chains
    }
}
