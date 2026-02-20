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

    pub(crate) fn clients_mut(&mut self) -> &mut HashMap<Client, ClientInfo> {
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
    active_chains: usize,
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
            active_chains: 0,
        }
    }

    pub(crate) fn client_veth(&self) -> IpAddr {
        self.client_veth
    }

    pub(crate) fn server_veth(&self) -> IpAddr {
        self.server_veth
    }

    pub(crate) fn vlan_id(&self) -> u16 {
        self.vlan_id
    }

    pub(crate) fn time_ms(&self) -> u128 {
        self.time_ms
    }

    pub(crate) fn add_active_chain(&mut self) {
        self.active_chains += 1;
    }

    pub(crate) fn remove_active_chains(&mut self, num_chains: usize) {
        self.active_chains = self.active_chains.saturating_sub(num_chains);
    }

    pub(crate) fn active_chains(&self) -> usize {
        self.active_chains
    }
}
