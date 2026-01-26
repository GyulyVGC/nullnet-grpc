use crate::proto::nullnet_grpc::{Empty, HostMapping, VlanSetup};
use nullnet_liberror::{Error, ErrorHandler, Location, location};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};
use tonic::{Status, Streaming};

pub(crate) type OutboundStream = mpsc::Sender<Result<VlanSetup, Status>>;
pub(crate) type InboundStream = Streaming<Empty>;

#[derive(Debug, Clone)]
pub struct Orchestrator {
    pub(crate) clients: Arc<Mutex<HashMap<IpAddr, (InboundStream, OutboundStream)>>>,
}

impl Orchestrator {
    pub fn new() -> Self {
        Self {
            clients: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub(crate) async fn add_client(
        &self,
        client_ip: IpAddr,
        inbound: InboundStream,
        outbound: OutboundStream,
    ) {
        let mut clients = self.clients.lock().await;
        clients.insert(client_ip, (inbound, outbound));
    }

    pub(crate) async fn send_vlan_setup_requests(
        &self,
        target_ip: IpAddr,
        veth_ip: IpAddr,
        vlan_id: u16,
        destinations: &Vec<IpAddr>,
        host_mapping: Option<HostMapping>,
    ) -> Result<(), Error> {
        let msg = VlanSetup {
            target_ip: target_ip.to_string(),
            vlan_id: u32::from(vlan_id),
            veth_ip: veth_ip.to_string(),
            host_mapping,
        };

        let mut clients = self.clients.lock().await;
        for dest in destinations {
            let Some((inbound, outbound)) = clients.get_mut(dest) else {
                continue;
            };

            println!(
                "VLAN setup request for {target_ip}: veth {veth_ip} on VLAN {vlan_id} (sending to {dest})"
            );

            outbound
                .send(Ok(msg.clone()))
                .await
                .handle_err(location!())?;

            let _ = inbound.message().await;

            println!("{dest} acknowledged");
        }

        Ok(())
    }
}
