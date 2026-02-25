use crate::proto::nullnet_grpc::{MsgId, VxlanMessage};
use nullnet_liberror::{Error, ErrorHandler, Location, location};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock, mpsc, oneshot};
use tonic::{Request, Status, Streaming};
use uuid::Uuid;

type OutboundStream = mpsc::Sender<Result<VxlanMessage, Status>>;

#[derive(Debug, Clone)]
pub struct Orchestrator {
    clients: Arc<RwLock<HashMap<IpAddr, OutboundStream>>>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<()>>>>,
}

impl Orchestrator {
    pub fn new() -> Self {
        Self {
            clients: Arc::new(RwLock::new(HashMap::new())),
            pending: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub(crate) async fn add_client(
        &self,
        request: Request<Streaming<MsgId>>,
        outbound: OutboundStream,
    ) -> Result<(), Error> {
        let client_ip = request
            .remote_addr()
            .ok_or("Could not get remote address for control channel request")
            .handle_err(location!())?
            .ip();

        self.clients.write().await.insert(client_ip, outbound);

        let mut inbound = request.into_inner();
        let orchestrator = self.clone();
        tokio::spawn(async move {
            while let Ok(Some(msg_id)) = inbound.message().await {
                if let Some(tx) = orchestrator.pending.lock().await.remove(&msg_id.id) {
                    let _ = tx.send(());
                }
            }
            println!("Control channel from '{client_ip}' closed");
        });

        Ok(())
    }

    pub(crate) async fn send_vxlan_setup(
        &self,
        client_ip: IpAddr,
        mut vxlan_setup: VxlanMessage,
    ) -> Result<(), Error> {
        let clients = self.clients.read().await;
        if let Some(outbound) = clients.get(&client_ip) {
            let (tx, rx) = oneshot::channel();
            let id = Uuid::new_v4().to_string();
            self.pending.lock().await.insert(id.clone(), tx);
            vxlan_setup.msg_id = Some(MsgId { id });
            outbound
                .send(Ok(vxlan_setup))
                .await
                .handle_err(location!())?;
            rx.await.handle_err(location!())
        } else {
            Err(format!("Client with IP {client_ip} not found")).handle_err(location!())
        }
    }
}
