use crate::proto::nullnet_grpc::{
    HostMapping, MsgId, VxlanMessage, VxlanSetup, VxlanTeardown, vxlan_message,
};
use crate::services::service_info::ServiceInfo;
use crate::vxlan::{cleanup_vxlans_chain, cleanup_vxlans_invalidated_service};
use ipnetwork::Ipv4Network;
use nullnet_liberror::{Error, ErrorHandler, Location, location};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;
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
            orchestrator
                .handle_node_disconnect(client_ip, &services)
                .await;
        });

        Ok(())
    }

    pub(crate) async fn remove_client(&self, ip: &IpAddr) {
        self.clients.write().await.remove(ip);
    }

    pub(crate) async fn handle_node_disconnect(
        &self,
        client_ip: IpAddr,
        services: &Arc<RwLock<HashMap<String, ServiceInfo>>>,
    ) {
        self.remove_client(&client_ip).await;

        let closed_services: Vec<String> = services
            .read()
            .await
            .iter()
            .filter_map(|(name, info)| {
                if let ServiceInfo::Registered(reg) = info
                    && reg.ip_port().0 == client_ip
                {
                    return Some(name.clone());
                }
                None
            })
            .collect();

        let mut services_guard = services.write().await;
        for closed_service in closed_services {
            let _ =
                cleanup_vxlans_invalidated_service(closed_service, true, &mut services_guard, self)
                    .await;
        }

        // clean up proxy chains from the disconnected node
        let proxy_services: Vec<String> = services_guard
            .iter()
            .filter(|(_, si)| {
                if let ServiceInfo::Registered(reg) = si {
                    reg.clients()
                        .keys()
                        .any(|c| c.is_proxy() == Some(client_ip))
                } else {
                    false
                }
            })
            .map(|(name, _)| name.clone())
            .collect();
        for name in proxy_services {
            let _ = cleanup_vxlans_chain(&name, &mut services_guard, self, Some(client_ip)).await;
        }
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
        let outbound = self.clients.read().await.get(&dest).cloned();
        if let Some(outbound) = outbound {
            let (tx, rx) = oneshot::channel();
            let id = Uuid::new_v4().to_string();
            self.pending.lock().await.insert(id.clone(), tx);

            let message = vxlan_message::Message::VxlanSetup(VxlanSetup {
                msg_id: Some(MsgId { id: id.clone() }),
                vxlan_id,
                ns_name: format!("ns_{vxlan_id}"),
                ns_net: ns_net.to_string(),
                br_name: format!("br_{vxlan_id}"),
                br_net: br_net.to_string(),
                local_ip: dest.to_string(),
                remote_ip: remote.to_string(),
                host_mapping,
            });

            if let Err(e) = outbound
                .send(Ok(VxlanMessage {
                    message: Some(message),
                }))
                .await
            {
                self.pending.lock().await.remove(&id);
                return Err(e.to_string()).handle_err(location!());
            }

            if let Ok(result) = tokio::time::timeout(Duration::from_secs(30), rx).await {
                result.handle_err(location!())
            } else {
                self.pending.lock().await.remove(&id);
                Err(format!("VXLAN setup ack timed out for {dest}")).handle_err(location!())
            }
        } else {
            Err(format!("Client with IP {dest} not found")).handle_err(location!())
        }
    }

    pub(crate) async fn send_vxlan_teardown(
        &self,
        dest: IpAddr,
        vxlan_id: u32,
    ) -> Result<(), Error> {
        let outbound = self.clients.read().await.get(&dest).cloned();
        if let Some(outbound) = outbound {
            println!("Sending VXLAN {vxlan_id} teardown to client {dest}");

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

#[cfg(test)]
impl Orchestrator {
    pub(crate) async fn register_fake_client(&self, ip: IpAddr) {
        use crate::proto::nullnet_grpc::vxlan_message;

        let (tx, mut rx) = mpsc::channel::<Result<VxlanMessage, Status>>(64);
        self.clients.write().await.insert(ip, tx);

        let pending = self.pending.clone();
        tokio::spawn(async move {
            while let Some(Ok(msg)) = rx.recv().await {
                // auto-ack VxlanSetup messages
                if let Some(vxlan_message::Message::VxlanSetup(setup)) = msg.message {
                    if let Some(msg_id) = setup.msg_id {
                        if let Some(tx) = pending.lock().await.remove(&msg_id.id) {
                            let _ = tx.send(());
                        }
                    }
                }
            }
        });
    }
}
