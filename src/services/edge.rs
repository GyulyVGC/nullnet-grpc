use crate::services::clients::Client;
use std::net::IpAddr;

pub(crate) struct Edge {
    pub(crate) client: (Option<IpAddr>, Client),
    pub(crate) server: (Option<IpAddr>, Client),
}

impl Edge {
    pub(crate) fn new(
        client_ip: Option<IpAddr>,
        client: Client,
        server_ip: Option<IpAddr>,
        server: Client,
    ) -> Self {
        Self {
            client: (client_ip, client),
            server: (server_ip, server),
        }
    }

    pub(crate) fn into_registered(self) -> Option<RegisteredEdge> {
        if let (Some(client_ip), Some(server_ip)) = (self.client.0, self.server.0) {
            Some(RegisteredEdge {
                client: (client_ip, self.client.1),
                server: (server_ip, self.server.1),
            })
        } else {
            None
        }
    }
}

pub(crate) struct RegisteredEdge {
    pub(crate) client: (IpAddr, Client),
    pub(crate) server: (IpAddr, Client),
}

impl RegisteredEdge {
    pub(crate) fn new(
        client_ip: IpAddr,
        client: Client,
        server_ip: IpAddr,
        server: Client,
    ) -> Self {
        Self {
            client: (client_ip, client),
            server: (server_ip, server),
        }
    }
}
