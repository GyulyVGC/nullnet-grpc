use crate::services::clients::Client;
use std::net::IpAddr;

pub(crate) struct Edge {
    pub(crate) client: (Option<IpAddr>, Client),
    pub(crate) server: (Option<IpAddr>, Client),
    pub(crate) client_docker: Option<String>,
    pub(crate) server_docker: Option<String>,
}

impl Edge {
    pub(crate) fn new(
        client_ip: Option<IpAddr>,
        client: Client,
        client_docker: Option<String>,
        server_ip: Option<IpAddr>,
        server: Client,
        server_docker: Option<String>,
    ) -> Self {
        Self {
            client: (client_ip, client),
            server: (server_ip, server),
            client_docker,
            server_docker,
        }
    }

    pub(crate) fn into_registered(self) -> Option<RegisteredEdge> {
        if let (Some(client_ip), Some(server_ip)) = (self.client.0, self.server.0) {
            Some(RegisteredEdge {
                client: (client_ip, self.client.1),
                server: (server_ip, self.server.1),
                client_docker: self.client_docker,
                server_docker: self.server_docker,
                backend_entry_port: None,
            })
        } else {
            None
        }
    }
}

pub(crate) struct RegisteredEdge {
    pub(crate) client: (IpAddr, Client),
    pub(crate) server: (IpAddr, Client),
    pub(crate) client_docker: Option<String>,
    pub(crate) server_docker: Option<String>,
    /// `Some(port)` iff this edge is the entry point of a backend-triggered
    /// chain. The port is the trigger port observed by the initiator and is
    /// echoed in the client-side `VxlanSetup.dnat_port` so the receiver can
    /// install DNAT(port -> `overlay_ip`).
    pub(crate) backend_entry_port: Option<u32>,
}

impl RegisteredEdge {
    pub(crate) fn new(
        client_ip: IpAddr,
        client: Client,
        client_docker: Option<String>,
        server_ip: IpAddr,
        server: Client,
        server_docker: Option<String>,
    ) -> Self {
        Self {
            client: (client_ip, client),
            server: (server_ip, server),
            client_docker,
            server_docker,
            backend_entry_port: None,
        }
    }
}
