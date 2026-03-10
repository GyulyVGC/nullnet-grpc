use crate::proto::nullnet_grpc::{HostMapping, MsgId, NetMessage, VxlanSetup, net_message};
use ipnetwork::Ipv4Network;
use nullnet_liberror::{ErrorHandler, Location, location};
use std::net::{IpAddr, Ipv4Addr};

pub(crate) enum Net {
    Vxlan,
    Vlan,
}

impl Net {
    pub(crate) fn setup(
        &self,
        id: String,
        dest: IpAddr,
        remote_server_name: Option<String>,
        vxlan_id: u32,
        remote: IpAddr,
    ) -> Option<(Ipv4Addr, NetMessage)> {
        match self {
            Net::Vxlan | Net::Vlan => {
                Self::vxlan_setup(id, dest, remote_server_name, vxlan_id, remote)
            }
        }
    }

    fn vxlan_setup(
        id: String,
        dest: IpAddr,
        remote_server_name: Option<String>,
        vxlan_id: u32,
        remote: IpAddr,
    ) -> Option<(Ipv4Addr, NetMessage)> {
        let [_, _, a, b] = vxlan_id.to_be_bytes();

        let (ns_net, br_net) = if remote_server_name.is_some() {
            // this is for client
            let ns_net_client = Ipv4Network::new(Ipv4Addr::new(10, a, b, 3), 24)
                .handle_err(location!())
                .ok()?;
            let br_net_client = Ipv4Network::new(Ipv4Addr::new(10, a, b, 4), 24)
                .handle_err(location!())
                .ok()?;
            (ns_net_client, br_net_client)
        } else {
            // this is for server
            let ns_net_server = Ipv4Network::new(Ipv4Addr::new(10, a, b, 1), 24)
                .handle_err(location!())
                .ok()?;
            let br_net_server = Ipv4Network::new(Ipv4Addr::new(10, a, b, 2), 24)
                .handle_err(location!())
                .ok()?;
            (ns_net_server, br_net_server)
        };

        let host_mapping = remote_server_name.map(|name| {
            HostMapping {
                ip: br_net.ip().to_string(),
                name,
            }
        });

        Some((
            br_net.ip(),
            NetMessage {
                message: Some(net_message::Message::VxlanSetup(VxlanSetup {
                    msg_id: Some(MsgId { id }),
                    vxlan_id,
                    ns_name: format!("ns_{vxlan_id}"),
                    ns_net: ns_net.to_string(),
                    br_name: format!("br_{vxlan_id}"),
                    br_net: br_net.to_string(),
                    local_ip: dest.to_string(),
                    remote_ip: remote.to_string(),
                    host_mapping,
                })),
            },
        ))
    }
}
