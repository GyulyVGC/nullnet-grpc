use crate::proto::nullnet_grpc::{
    HostMapping, MsgId, Net, NetMessage, VlanSetup, VxlanSetup, net_message,
};
use ipnetwork::Ipv4Network;
use nullnet_liberror::{ErrorHandler, Location, location};
use std::net::{IpAddr, Ipv4Addr};

impl Net {
    pub(crate) fn setup(
        self,
        id: String,
        dest: IpAddr,
        remote_server_name: Option<String>,
        vxlan_id: u32,
        remote: IpAddr,
    ) -> Option<(Ipv4Addr, NetMessage)> {
        match self {
            Net::Vlan => Self::vlan_setup(id, dest, remote_server_name, vxlan_id, remote),
            Net::Vxlan => Self::vxlan_setup(id, dest, remote_server_name, vxlan_id, remote),
        }
    }

    #[allow(clippy::unnecessary_wraps)]
    fn vlan_setup(
        id: String,
        dest: IpAddr,
        remote_server_name: Option<String>,
        vlan_id: u32,
        remote: IpAddr,
    ) -> Option<(Ipv4Addr, NetMessage)> {
        let [_, _, a, b] = vlan_id.to_be_bytes();

        let (local_veth, remote_veth) = if remote_server_name.is_some() {
            // this is for client
            let remote_veth = Ipv4Addr::new(10, a, b, 1);
            let local_veth = Ipv4Addr::new(10, a, b, 2);
            (local_veth, remote_veth)
        } else {
            // this is for server
            let local_veth = Ipv4Addr::new(10, a, b, 1);
            let remote_veth = Ipv4Addr::new(10, a, b, 2);
            (local_veth, remote_veth)
        };

        let host_mapping = remote_server_name.map(|name| HostMapping {
            ip: remote_veth.to_string(),
            name,
        });

        Some((
            remote_veth,
            NetMessage {
                message: Some(net_message::Message::VlanSetup(VlanSetup {
                    msg_id: Some(MsgId { id }),
                    vlan_id,
                    local_veth: local_veth.to_string(),
                    remote_veth: remote_veth.to_string(),
                    local_ip: dest.to_string(),
                    remote_ip: remote.to_string(),
                    host_mapping,
                })),
            },
        ))
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

        let host_mapping = remote_server_name.map(|name| HostMapping {
            ip: br_net.ip().to_string(),
            name,
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
