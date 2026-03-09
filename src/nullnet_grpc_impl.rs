use crate::graphviz::generate_graphviz;
use crate::orchestrator::Orchestrator;
use crate::proto::nullnet_grpc::nullnet_grpc_server::NullnetGrpc;
use crate::proto::nullnet_grpc::{
    Empty, HostMapping, MsgId, ProxyRequest, Services, Upstream, VxlanMessage,
};
use crate::services::clients::{Client, ClientInfo};
use crate::services::input::ServicesToml;
use crate::services::service_info::ServiceInfo;
use crate::vxlan::cleanup_vxlans_invalidated_service;
use ipnetwork::Ipv4Network;
use nullnet_liberror::{Error, ErrorHandler, Location, location};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock, mpsc};
use tokio::task::JoinSet;
use tonic::codegen::tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

pub(crate) struct NullnetGrpcImpl {
    /// The available services
    services: Arc<RwLock<HashMap<String, ServiceInfo>>>,
    /// Last registered VXLAN ID
    last_registered_vxlan: Arc<Mutex<u32>>,
    /// Orchestrator to manage TAP-based clients and VXLAN setups
    orchestrator: Orchestrator,
}

impl NullnetGrpcImpl {
    pub async fn new() -> Result<Self, Error> {
        let services = Arc::new(RwLock::new(ServicesToml::load().await?));

        // regenerate the service graphviz periodically for debugging
        let services_2 = services.clone();
        tokio::spawn(async move {
            generate_graphviz(services_2).await;
        });

        let orchestrator = Orchestrator::new();
        let orchestrator_2 = orchestrator.clone();

        // keep services up to date with the services.toml file
        let services_2 = services.clone();
        tokio::spawn(async move {
            ServicesToml::watch(&services_2, orchestrator_2.clone())
                .await
                .expect("failed to watch services.toml for changes");
        });

        Ok(NullnetGrpcImpl {
            services,
            last_registered_vxlan: Arc::new(Mutex::new(100)),
            orchestrator,
        })
    }

    async fn control_channel_impl(
        &self,
        request: Request<Streaming<MsgId>>,
    ) -> Result<Response<<NullnetGrpcImpl as NullnetGrpc>::ControlChannelStream>, Error> {
        let (outbound, receiver) = mpsc::channel(64);

        self.orchestrator
            .add_client(request, outbound, self.services.clone())
            .await?;

        Ok(Response::new(ReceiverStream::new(receiver)))
    }

    // TODO: avoid race conditions when multiple proxy requests are made concurrently
    async fn proxy_impl(
        &self,
        request: Request<ProxyRequest>,
    ) -> Result<Response<Upstream>, Error> {
        let proxy_ip = request
            .remote_addr()
            .ok_or("Could not get remote address for proxy request")
            .handle_err(location!())?
            .ip();

        let req = request.into_inner();

        let client_ip: IpAddr = req.client_ip.parse().handle_err(location!())?;
        let service_name = req.service_name;

        println!("Received proxy request for '{service_name}'");

        let service_info = self
            .services
            .read()
            .await
            .get(&service_name)
            .cloned()
            .ok_or("Service not found")
            .handle_err(location!())?;

        if !service_info.is_proxy_reachable() {
            Err("Service is not reachable via proxy").handle_err(location!())?;
        }

        let ServiceInfo::Registered(registered) = service_info else {
            Err("Service is not registered").handle_err(location!())?
        };

        let proxy_client = Client::new(client_ip.to_string(), Some(proxy_ip));
        if let Some(upstream) = registered.is_client_setup(&proxy_client) {
            println!("'{client_ip}' ---> '{service_name}' is already set up");
            return Ok(Response::new(upstream));
        }

        let (_, service_port) = registered.ip_port();
        let upstream_ip = self
            .setup_proxy_chain(&service_name, proxy_ip, &client_ip.to_string())
            .await?;

        Ok(Response::new(Upstream {
            ip: upstream_ip.to_string(),
            port: u32::from(service_port),
        }))
    }

    async fn services_list_impl(
        &self,
        request: Request<Services>,
    ) -> Result<Response<Empty>, Error> {
        let sender_ip = request
            .remote_addr()
            .ok_or("Could not get remote address for services list request")
            .handle_err(location!())?
            .ip();

        let req = request.into_inner();

        println!(
            "Received services list from '{}': {:?}",
            sender_ip, req.services
        );

        let service_list: Vec<(String, u16)> = req
            .services
            .into_iter()
            .map(|s| Ok((s.name, u16::try_from(s.port).handle_err(location!())?)))
            .collect::<Result<_, Error>>()?;

        self.apply_services_list(sender_ip, &service_list).await?;

        Ok(Response::new(Empty {}))
    }

    pub(crate) async fn setup_proxy_chain(
        &self,
        service_name: &str,
        proxy_ip: IpAddr,
        client_ip: &str,
    ) -> Result<Ipv4Addr, Error> {
        let proxy_client = Client::new(client_ip.to_string(), Some(proxy_ip));

        let guard = self.services.read().await;
        let service_info = guard
            .get(service_name)
            .ok_or("Service not found")
            .handle_err(location!())?;
        let ServiceInfo::Registered(registered) = service_info else {
            Err("Service is not registered").handle_err(location!())?
        };
        let service_ip = registered.ip_port().0;
        let dep_chain = registered.dependency_chain(service_name.to_string(), &guard);
        drop(guard);

        let mut dep_chain: Vec<((IpAddr, Client), (IpAddr, Client))> = dep_chain
            .into_iter()
            .map(|((cip, c), (sip, s))| {
                Ok((
                    (
                        cip.ok_or("Dependency not registered")
                            .handle_err(location!())?,
                        c,
                    ),
                    (
                        sip.ok_or("Dependency not registered")
                            .handle_err(location!())?,
                        s,
                    ),
                ))
            })
            .collect::<Result<_, Error>>()?;

        dep_chain.push((
            (proxy_ip, proxy_client),
            (service_ip, Client::new(service_name.to_string(), None)),
        ));

        self.vxlan_chain_setup(dep_chain).await
    }

    pub(crate) async fn apply_services_list(
        &self,
        sender_ip: IpAddr,
        service_list: &[(String, u16)],
    ) -> Result<(), Error> {
        // get services previously registered from this sender_ip
        let previously_registered: Vec<String> = self
            .services
            .read()
            .await
            .iter()
            .filter_map(|(name, si)| {
                if let ServiceInfo::Registered(reg) = si {
                    let (ip, _) = reg.ip_port();
                    if ip == sender_ip {
                        return Some(name.clone());
                    }
                }
                None
            })
            .collect();

        // get services that are no longer present
        let to_be_unregistered: Vec<String> = previously_registered
            .into_iter()
            .filter(|name| !service_list.iter().any(|(s, _)| s == name))
            .collect();

        let mut services_mut = self.services.write().await;

        // unregister services that are no longer present
        for service_name in to_be_unregistered {
            cleanup_vxlans_invalidated_service(
                service_name.clone(),
                true,
                &mut services_mut,
                &self.orchestrator,
            )
            .await?;
        }

        // re-register services that are still present
        for (name, port) in service_list {
            services_mut.entry(name.clone()).and_modify(|si| {
                si.register(sender_ip, *port);
            });
        }

        Ok(())
    }

    pub(crate) async fn vxlan_chain_setup(
        &self,
        dep_chain: Vec<((IpAddr, Client), (IpAddr, Client))>,
    ) -> Result<Ipv4Addr, Error> {
        let mut join_set_outer = JoinSet::new();
        for ((client_ethernet, client), (server_ethernet, server)) in dep_chain {
            let services = self.services.clone();
            let orchestrator = self.orchestrator.clone();
            let last_registered_vxlan = self.last_registered_vxlan.clone();
            join_set_outer.spawn(async move {
                let init_time = std::time::Instant::now();

                // add chain and check if the link is already set up
                let mut services_guard = services.write().await;
                let Some(ServiceInfo::Registered(reg)) = services_guard.get_mut(server.name())
                else {
                    return None;
                };
                if reg.is_client_setup(&client).is_some() {
                    reg.add_chain(&client);
                    return None;
                }
                // reserve the slot so concurrent requests see it as in-progress
                reg.add_client(client.clone(), ClientInfo::placeholder());
                drop(services_guard);

                // TODO: check for VXLAN ID overflow (max 65535) and reclaim freed IDs
                let mut last_id = last_registered_vxlan.lock().await;
                *last_id += 1;
                let vxlan_id = *last_id;
                drop(last_id);

                let [_, _, a, b] = vxlan_id.to_be_bytes();
                let ns_net_server = Ipv4Network::new(Ipv4Addr::new(10, a, b, 1), 24)
                    .handle_err(location!())
                    .ok()?;
                let br_net_server = Ipv4Network::new(Ipv4Addr::new(10, a, b, 2), 24)
                    .handle_err(location!())
                    .ok()?;
                let ns_net_client = Ipv4Network::new(Ipv4Addr::new(10, a, b, 3), 24)
                    .handle_err(location!())
                    .ok()?;
                let br_net_client = Ipv4Network::new(Ipv4Addr::new(10, a, b, 4), 24)
                    .handle_err(location!())
                    .ok()?;

                let br_ip_server = br_net_server.ip();
                let br_ip_client = br_net_client.ip();

                let host_mapping = Some(HostMapping {
                    ip: br_ip_server.to_string(),
                    name: server.name().to_string(),
                });

                let upstream_ip = if client.is_proxy().is_some() {
                    Some(br_ip_server)
                } else {
                    None
                };

                let orch = orchestrator.clone();
                let server_res = orch.send_vxlan_setup(
                    server_ethernet,
                    vxlan_id,
                    ns_net_server,
                    br_net_server,
                    client_ethernet,
                    None,
                );
                let orch2 = orchestrator.clone();
                let client_res = orch2.send_vxlan_setup(
                    client_ethernet,
                    vxlan_id,
                    ns_net_client,
                    br_net_client,
                    server_ethernet,
                    host_mapping,
                );

                let (server_ok, client_ok) = tokio::join!(server_res, client_res);

                if server_ok.is_err() || client_ok.is_err() {
                    // rollback: teardown whichever side succeeded
                    if server_ok.is_ok() {
                        let _ = orchestrator
                            .send_vxlan_teardown(server_ethernet, vxlan_id)
                            .await;
                    }
                    if client_ok.is_ok() {
                        let _ = orchestrator
                            .send_vxlan_teardown(client_ethernet, vxlan_id)
                            .await;
                    }
                    // remove placeholder
                    if let Some(ServiceInfo::Registered(reg)) =
                        services.write().await.get_mut(server.name())
                    {
                        reg.clients_mut().remove(&client);
                    }
                    return None;
                }

                println!("{server_ethernet} acknowledged");
                println!("{client_ethernet} acknowledged");

                // register the link between the two services
                let mut guard = services.write().await;
                if let Some(ServiceInfo::Registered(reg)) = guard.get_mut(server.name()) {
                    let time_ms = init_time.elapsed().as_millis();
                    let ci = ClientInfo::new(br_ip_client, br_ip_server, vxlan_id, time_ms);
                    reg.add_client(client.clone(), ci);
                    reg.add_chain(&client);
                } else {
                    // service was unregistered during setup — teardown VXLANs
                    drop(guard);
                    let _ = orchestrator
                        .send_vxlan_teardown(server_ethernet, vxlan_id)
                        .await;
                    let _ = orchestrator
                        .send_vxlan_teardown(client_ethernet, vxlan_id)
                        .await;
                }

                upstream_ip
            });
        }

        let mut ret_val = None;
        while let Some(res) = join_set_outer.join_next().await {
            if let Ok(Some(upstream_ip)) = res {
                ret_val = Some(upstream_ip);
            }
        }

        ret_val
            .ok_or("No valid upstream IP found after VXLAN chain setup")
            .handle_err(location!())
    }
}

#[cfg(test)]
impl NullnetGrpcImpl {
    pub(crate) fn new_for_test(services: HashMap<String, ServiceInfo>) -> Self {
        NullnetGrpcImpl {
            services: Arc::new(RwLock::new(services)),
            last_registered_vxlan: Arc::new(Mutex::new(100)),
            orchestrator: Orchestrator::new(),
        }
    }

    pub(crate) fn orchestrator(&self) -> &Orchestrator {
        &self.orchestrator
    }

    pub(crate) fn services(&self) -> &Arc<RwLock<HashMap<String, ServiceInfo>>> {
        &self.services
    }
}

#[tonic::async_trait]
impl NullnetGrpc for NullnetGrpcImpl {
    type ControlChannelStream = ReceiverStream<Result<VxlanMessage, Status>>;

    async fn control_channel(
        &self,
        request: Request<Streaming<MsgId>>,
    ) -> Result<Response<Self::ControlChannelStream>, Status> {
        println!(
            "Nullnet control channel requested from '{}'",
            request
                .remote_addr()
                .map_or("unknown".into(), |addr| addr.ip().to_string())
        );

        self.control_channel_impl(request)
            .await
            .map_err(|err| Status::internal(err.to_str()))
    }

    async fn services_list(&self, req: Request<Services>) -> Result<Response<Empty>, Status> {
        self.services_list_impl(req)
            .await
            .map_err(|err| Status::internal(err.to_str()))
    }

    async fn proxy(&self, req: Request<ProxyRequest>) -> Result<Response<Upstream>, Status> {
        self.proxy_impl(req)
            .await
            .map_err(|err| Status::internal(err.to_str()))
    }
}
