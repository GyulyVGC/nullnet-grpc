use crate::graphviz::generate_graphviz;
use crate::orchestrator::Orchestrator;
use crate::proto::nullnet_grpc::nullnet_grpc_server::NullnetGrpc;
use crate::proto::nullnet_grpc::{
    Empty, MsgId, Net, NetMessage, NetType, ProxyRequest, Services, Upstream,
};
use crate::services::changes::{apply_changes, detect_services_list_changes};
use crate::services::clients::{Client, ClientInfo};
use crate::services::input::ServicesToml;
use crate::services::service_info::ServiceInfo;
use nullnet_liberror::{Error, ErrorHandler, Location, location};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock, mpsc};
use tokio::task::JoinSet;
use tonic::codegen::tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

pub(crate) struct NullnetGrpcImpl {
    /// The network type to use
    net_type: Net,
    /// The available services
    services: Arc<RwLock<HashMap<String, ServiceInfo>>>,
    /// Last registered NET ID
    last_registered_net: Arc<Mutex<u32>>,
    /// Orchestrator to manage TAP-based clients and NET setups
    orchestrator: Orchestrator,
}

impl NullnetGrpcImpl {
    pub async fn new() -> Result<Self, Error> {
        // TODO: read env at runtime
        let net_type = option_env!("NET_TYPE").unwrap_or("VLAN");
        let net_type = match net_type.to_uppercase().as_str() {
            "VXLAN" => Net::Vxlan,
            "VLAN" => Net::Vlan,
            other => return Err(format!("Unsupported NET_TYPE: {other}")).handle_err(location!()),
        };

        let services = Arc::new(RwLock::new(ServicesToml::load().await?));

        // regenerate the service graphviz periodically for debugging
        let services_2 = services.clone();
        tokio::spawn(async move {
            generate_graphviz(services_2, net_type).await;
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
            net_type,
            services,
            last_registered_net: Arc::new(Mutex::new(100)),
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

        self.net_chain_setup(dep_chain).await
    }

    pub(crate) async fn apply_services_list(
        &self,
        sender_ip: IpAddr,
        service_list: &[(String, u16)],
    ) -> Result<(), Error> {
        let mut services_mut = self.services.write().await;

        let changes = detect_services_list_changes(&services_mut, sender_ip, service_list);
        apply_changes(changes, &mut services_mut, None, &self.orchestrator).await;

        // re-register services that are still present
        for (name, port) in service_list {
            services_mut.entry(name.clone()).and_modify(|si| {
                si.register(sender_ip, *port);
            });
        }

        Ok(())
    }

    pub(crate) async fn net_chain_setup(
        &self,
        dep_chain: Vec<((IpAddr, Client), (IpAddr, Client))>,
    ) -> Result<Ipv4Addr, Error> {
        let mut join_set_outer = JoinSet::new();
        for ((client_ethernet, client), (server_ethernet, server)) in dep_chain {
            let services = self.services.clone();
            let orchestrator = self.orchestrator.clone();
            let last_registered_net = self.last_registered_net.clone();
            let net_type = self.net_type;
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

                // TODO: check for NET ID overflow (max 65535) and reclaim freed IDs
                let mut last_id = last_registered_net.lock().await;
                *last_id += 1;
                let net_id = *last_id;
                drop(last_id);

                let orch = orchestrator.clone();
                let server_res =
                    orch.send_net_setup(net_type, server_ethernet, None, net_id, client_ethernet);
                let orch2 = orchestrator.clone();
                let client_res = orch2.send_net_setup(
                    net_type,
                    client_ethernet,
                    Some(server.name().to_string()),
                    net_id,
                    server_ethernet,
                );

                let (server_ok, client_ok) = tokio::join!(server_res, client_res);

                if server_ok.is_none() || client_ok.is_none() {
                    // rollback: teardown whichever side succeeded
                    if server_ok.is_some() {
                        let _ = orchestrator
                            .send_net_teardown(server_ethernet, net_id)
                            .await;
                    }
                    if client_ok.is_some() {
                        let _ = orchestrator
                            .send_net_teardown(client_ethernet, net_id)
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

                let net_ip_server = server_ok?;
                let net_ip_client = client_ok?;

                println!("{server_ethernet} acknowledged");
                println!("{client_ethernet} acknowledged");

                // register the link between the two services
                let mut guard = services.write().await;
                if let Some(ServiceInfo::Registered(reg)) = guard.get_mut(server.name()) {
                    let time_ms = init_time.elapsed().as_millis();
                    let ci = ClientInfo::new(net_ip_client, net_ip_server, net_id, time_ms);
                    reg.add_client(client.clone(), ci);
                    reg.add_chain(&client);
                } else {
                    // service was unregistered during setup — teardown NETs
                    drop(guard);
                    let _ = orchestrator
                        .send_net_teardown(server_ethernet, net_id)
                        .await;
                    let _ = orchestrator
                        .send_net_teardown(client_ethernet, net_id)
                        .await;
                }

                if client.is_proxy().is_some() {
                    Some(net_ip_server)
                } else {
                    None
                }
            });
        }

        let mut ret_val = None;
        while let Some(res) = join_set_outer.join_next().await {
            if let Ok(Some(upstream_ip)) = res {
                ret_val = Some(upstream_ip);
            }
        }

        ret_val
            .ok_or("No valid upstream IP found after NET chain setup")
            .handle_err(location!())
    }
}

#[cfg(test)]
impl NullnetGrpcImpl {
    pub(crate) fn new_for_test(services: HashMap<String, ServiceInfo>) -> Self {
        NullnetGrpcImpl {
            net_type: Net::Vlan,
            services: Arc::new(RwLock::new(services)),
            last_registered_net: Arc::new(Mutex::new(100)),
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
    async fn network_type(&self, _: Request<Empty>) -> Result<Response<NetType>, Status> {
        Ok(Response::new(NetType {
            net: self.net_type.into(),
        }))
    }

    async fn services_list(&self, req: Request<Services>) -> Result<Response<Empty>, Status> {
        self.services_list_impl(req)
            .await
            .map_err(|err| Status::internal(err.to_str()))
    }

    type ControlChannelStream = ReceiverStream<Result<NetMessage, Status>>;

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

    async fn proxy(&self, req: Request<ProxyRequest>) -> Result<Response<Upstream>, Status> {
        self.proxy_impl(req)
            .await
            .map_err(|err| Status::internal(err.to_str()))
    }
}
