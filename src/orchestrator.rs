use crate::proto::nullnet_grpc::{
    HostMapping, MsgId, VxlanMessage, VxlanSetup, VxlanTeardown, vxlan_message,
};
use crate::services::service_info::ServiceInfo;
use crate::vxlan::cleanup_vxlans_invalidated_service;
use ipnetwork::Ipv4Network;
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
        services: Arc<RwLock<HashMap<String, ServiceInfo>>>,
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
            // remove client from orchestrator state
            orchestrator.clients.write().await.remove(&client_ip);

            // get all services running on client_ip
            let closed_services: Vec<String> = services
                .read()
                .await
                .iter()
                .filter(|(_, info)| {
                    if let ServiceInfo::Registered(reg) = info {
                        let (reg_ip, _) = reg.ip_port();
                        reg_ip == client_ip
                    } else {
                        false
                    }
                })
                .map(|(name, _)| name.clone())
                .collect();

            // cleanup VXLANs for all closed services
            for closed_service in closed_services {
                let _ = cleanup_vxlans_invalidated_service(
                    closed_service,
                    true,
                    &mut *services.write().await,
                    &orchestrator,
                )
                .await;
            }
        });

        Ok(())
    }

    pub(crate) async fn send_vxlan_setup(
        &self,
        dest: IpAddr,
        vxlan_id: u32,
        ns_net: Ipv4Network,
        br_net: Ipv4Network,
        remote: IpAddr,
        host_mapping: Option<HostMapping>,
    ) -> Result<(), Error> {
        let clients = self.clients.read().await;
        if let Some(outbound) = clients.get(&dest) {
            let (tx, rx) = oneshot::channel();
            let id = Uuid::new_v4().to_string();
            self.pending.lock().await.insert(id.clone(), tx);

            let message = vxlan_message::Message::VxlanSetup(VxlanSetup {
                msg_id: Some(MsgId { id }),
                vxlan_id,
                ns_name: format!("ns_{vxlan_id}"),
                ns_net: ns_net.to_string(),
                br_name: format!("br_{vxlan_id}"),
                br_net: br_net.to_string(),
                local_ip: dest.to_string(),
                remote_ip: remote.to_string(),
                host_mapping,
            });

            outbound
                .send(Ok(VxlanMessage {
                    message: Some(message),
                }))
                .await
                .handle_err(location!())?;
            rx.await.handle_err(location!())
        } else {
            Err(format!("Client with IP {dest} not found")).handle_err(location!())
        }
    }

    pub(crate) async fn send_vxlan_teardown(
        &self,
        dest: IpAddr,
        vxlan_id: u32,
    ) -> Result<(), Error> {
        let clients = self.clients.read().await;
        if let Some(outbound) = clients.get(&dest) {
            let message = vxlan_message::Message::VxlanTeardown(VxlanTeardown {
                ns_name: format!("ns_{vxlan_id}"),
                br_name: format!("br_{vxlan_id}"),
            });

            outbound
                .send(Ok(VxlanMessage {
                    message: Some(message),
                }))
                .await
                .handle_err(location!())?;
            Ok(())
        } else {
            Err(format!("Client with IP {dest} not found")).handle_err(location!())
        }
    }
}
