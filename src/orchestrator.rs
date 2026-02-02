use crate::proto::nullnet_grpc::{Empty, VlanSetup};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock, mpsc};
use tonic::{Status, Streaming};

pub(crate) type OutboundStream = mpsc::Sender<Result<VlanSetup, Status>>;
pub(crate) type InboundStream = Streaming<Empty>;

#[derive(Debug, Clone)]
pub struct Orchestrator {
    // Use RwLock for concurrent reads to parallelize VLAN setup requests
    pub(crate) clients: Arc<RwLock<HashMap<IpAddr, Arc<Mutex<(InboundStream, OutboundStream)>>>>>,
}

impl Orchestrator {
    pub fn new() -> Self {
        Self {
            clients: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub(crate) async fn add_client(
        &self,
        client_ip: IpAddr,
        inbound: InboundStream,
        outbound: OutboundStream,
    ) {
        self.clients
            .write()
            .await
            .insert(client_ip, Arc::new(Mutex::new((inbound, outbound))));
    }
}
