use crate::env::NET_TYPE;
use crate::graphviz::generate_graphviz;
use crate::orchestrator::Orchestrator;
use crate::proto::nullnet_grpc::nullnet_grpc_server::NullnetGrpc;
use crate::proto::nullnet_grpc::{
    Empty, MsgId, NetMessage, NetType, ProxyRequest, Services, Upstream,
};
use crate::services::changes::{
    apply_changes, collect_dep_chain_edges, detect_services_list_changes,
};
use crate::services::clients::{Client, ClientInfo};
use crate::services::edge::RegisteredEdge;
use crate::services::input::ServicesToml;
use crate::services::service_info::ServiceInfo;
use crate::timeout::check_proxy_timeouts;
use nullnet_liberror::{Error, ErrorHandler, Location, location};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use tokio::sync::{Notify, RwLock, mpsc};
use tokio::task::JoinSet;
use tonic::codegen::tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

pub(crate) struct NullnetGrpcImpl {
    /// The available services
    services: Arc<RwLock<HashMap<String, ServiceInfo>>>,
    /// Orchestrator to manage TAP-based clients and NET setups
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
        let config_changed = Arc::new(Notify::new());

        // keep services up to date with the services.toml file
        let services_2 = services.clone();
        let orchestrator_2 = orchestrator.clone();
        let config_changed_2 = config_changed.clone();
        tokio::spawn(async move {
            ServicesToml::watch(&services_2, orchestrator_2, config_changed_2)
                .await
                .expect("failed to watch services.toml for changes");
        });

        // periodically check for timed-out proxy clients and tear down their chains
        let services_2 = services.clone();
        let orchestrator_2 = orchestrator.clone();
        tokio::spawn(async move {
            check_proxy_timeouts(services_2, orchestrator_2, config_changed).await;
        });

        Ok(NullnetGrpcImpl {
            services,
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

        if service_info.is_proxy_reachable().is_none() {
            Err("Service is not reachable via proxy").handle_err(location!())?;
        }

        let ServiceInfo::Registered(registered) = service_info else {
            Err("Service is not registered").handle_err(location!())?
        };

        let proxy_client = Client::new(client_ip.to_string(), Some(proxy_ip));

        // Sticky session: check if this client is already connected to a replica
        if let Some(upstream) = registered.is_client_setup(&proxy_client) {
            println!("'{client_ip}' ---> '{service_name}' is already set up");

            // update the latest timestamp for this client since it's being used again
            let mut services_mut = self.services.write().await;
            if let Some(ServiceInfo::Registered(reg)) = services_mut.get_mut(&service_name) {
                reg.set_latest_now(&proxy_client);
            }

            return Ok(Response::new(upstream));
        }

        // Max-networks: if the limit is reached, reuse the least-used existing
        // network on the same proxy instead of creating a new one.
        if let Some(max) = registered.max_networks()
            && registered.proxy_clients_count() >= max as usize
            && let Some((upstream, client_net, server_net, net_id, replica_ip, replica_docker)) =
                registered.find_reusable_network_on_proxy(proxy_ip)
        {
            println!(
                "Max networks ({max}) reached for '{service_name}', \
                 reusing network on proxy {proxy_ip}"
            );
            let mut services_mut = self.services.write().await;
            if let Some(ServiceInfo::Registered(reg)) = services_mut.get_mut(&service_name) {
                // Create a new Client entry sharing the existing network
                let new_ci = ClientInfo::new(proxy_ip, client_net, server_net, net_id, 0, None);
                reg.add_client_to_replica(
                    replica_ip,
                    replica_docker.as_deref(),
                    proxy_client.clone(),
                    new_ci,
                );
                reg.add_chain(&proxy_client);
            }
            // Increment chains on each dependency edge
            let dep_edges = collect_dep_chain_edges(
                &service_name,
                replica_ip,
                replica_docker.as_deref(),
                &services_mut,
            );
            for (dep_client, dep_name) in dep_edges {
                if let Some(ServiceInfo::Registered(dep_reg)) = services_mut.get_mut(&dep_name) {
                    dep_reg.add_chain(&dep_client);
                }
            }
            return Ok(Response::new(upstream));
        }

        self.new_proxy_chain(&service_name, proxy_ip, &client_ip.to_string())
            .await
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

        let service_list: Vec<(String, u16, Option<String>)> = req
            .services
            .into_iter()
            .map(|s| {
                Ok((
                    s.name,
                    u16::try_from(s.port).handle_err(location!())?,
                    s.docker_container,
                ))
            })
            .collect::<Result<_, Error>>()?;

        self.apply_services_list(sender_ip, &service_list).await?;

        Ok(Response::new(Empty {}))
    }

    pub(crate) async fn new_proxy_chain(
        &self,
        service_name: &str,
        proxy_ip: IpAddr,
        client_ip: &str,
    ) -> Result<Response<Upstream>, Error> {
        let guard = self.services.read().await;
        let reg = match guard.get(service_name) {
            Some(ServiceInfo::Registered(reg)) => reg,
            _ => Err("Service is not registered").handle_err(location!())?,
        };
        let replica = reg.pick_replica_least_clients();
        let service_ip = replica.ip();
        let service_port = replica.port();
        let service_docker = replica.docker_container().map(String::from);
        drop(guard);

        let upstream_ip = self
            .setup_proxy_chain(
                service_name,
                proxy_ip,
                client_ip,
                service_ip,
                service_docker.as_deref(),
            )
            .await?;

        Ok(Response::new(Upstream {
            ip: upstream_ip.to_string(),
            port: u32::from(service_port),
        }))
    }

    pub(crate) async fn setup_proxy_chain(
        &self,
        service_name: &str,
        proxy_ip: IpAddr,
        client_ip: &str,
        service_ip: IpAddr,
        service_docker: Option<&str>,
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
        let dep_chain = registered.dependency_chain(
            service_name.to_string(),
            service_ip,
            service_docker,
            &guard,
        );
        drop(guard);

        let mut dep_chain: Vec<RegisteredEdge> = dep_chain
            .into_iter()
            .map(|edge| {
                edge.into_registered()
                    .ok_or("Dependency not registered")
                    .handle_err(location!())
            })
            .collect::<Result<_, Error>>()?;

        dep_chain.push(RegisteredEdge::new(
            proxy_ip,
            proxy_client,
            None,
            service_ip,
            Client::new(service_name.to_string(), None),
            service_docker.map(String::from),
        ));

        self.net_chain_setup(dep_chain).await
    }

    pub(crate) async fn apply_services_list(
        &self,
        sender_ip: IpAddr,
        service_list: &[(String, u16, Option<String>)],
    ) -> Result<(), Error> {
        let mut services_mut = self.services.write().await;

        let changes = detect_services_list_changes(&services_mut, sender_ip, service_list);
        apply_changes(changes, &mut services_mut, None, &self.orchestrator).await;

        // add/update replicas for services that are present
        for (name, port, docker_container) in service_list {
            services_mut.entry(name.clone()).and_modify(|si| {
                si.add_replica(sender_ip, *port, docker_container.clone());
            });
        }

        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) async fn net_chain_setup(
        &self,
        dep_chain: Vec<RegisteredEdge>,
    ) -> Result<Ipv4Addr, Error> {
        let mut join_set_outer = JoinSet::new();
        for edge in dep_chain {
            let (client_ethernet, client) = edge.client;
            let (server_ethernet, server) = edge.server;
            let client_docker = edge.client_docker;
            let server_docker = edge.server_docker;

            let services = self.services.clone();
            let orchestrator = self.orchestrator.clone();
            join_set_outer.spawn(async move {
                let init_time = std::time::Instant::now();

                let mut services_guard = services.write().await;
                let Some(ServiceInfo::Registered(reg)) = services_guard.get_mut(server.name())
                else {
                    return None;
                };
                // Proxy edges: reuse if this client is already connected anywhere (stickiness).
                // Dep edges: reuse only if this exact (client, server_replica) pair exists,
                // so each proxy chain can independently pick a different replica.
                let already_setup = if client.is_proxy().is_some() {
                    reg.is_client_setup(&client).is_some()
                } else {
                    reg.is_client_on_replica(&client, server_ethernet, server_docker.as_deref())
                };
                if already_setup {
                    reg.add_chain(&client);
                    return None;
                }
                // reserve the slot so concurrent requests see it as in-progress
                reg.add_client_to_replica(
                    server_ethernet,
                    server_docker.as_deref(),
                    client.clone(),
                    ClientInfo::placeholder(client_ethernet),
                );

                drop(services_guard);

                let Some(net_id) = orchestrator.allocate_net_id().await else {
                    eprintln!("NET ID pool exhausted");
                    // remove placeholder
                    if let Some(ServiceInfo::Registered(reg)) =
                        services.write().await.get_mut(server.name())
                    {
                        reg.remove_client(&client);
                    }
                    return None;
                };

                let orch = orchestrator.clone();
                let cd = client_docker.clone();
                let sd = server_docker.clone();
                let server_res =
                    orch.send_net_setup(server_ethernet, None, net_id, client_ethernet, (cd, sd));
                let orch2 = orchestrator.clone();
                let cd = client_docker.clone();
                let sd = server_docker.clone();
                let client_res = orch2.send_net_setup(
                    client_ethernet,
                    Some(server.name().to_string()),
                    net_id,
                    server_ethernet,
                    (cd, sd),
                );

                let (server_ok, client_ok) = tokio::join!(server_res, client_res);

                if server_ok.is_none() || client_ok.is_none() {
                    // rollback
                    orchestrator
                        .send_net_teardown(
                            client_ethernet,
                            client_docker.clone(),
                            server_ethernet,
                            server_docker.clone(),
                            net_id,
                        )
                        .await;
                    // remove placeholder
                    if let Some(ServiceInfo::Registered(reg)) =
                        services.write().await.get_mut(server.name())
                    {
                        reg.remove_client(&client);
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
                    let ci = ClientInfo::new(
                        client_ethernet,
                        net_ip_client,
                        net_ip_server,
                        net_id,
                        time_ms,
                        client_docker.clone(),
                    );
                    reg.add_client_to_replica(
                        server_ethernet,
                        server_docker.as_deref(),
                        client.clone(),
                        ci,
                    );
                    reg.add_chain(&client);
                } else {
                    // service was unregistered during setup — teardown NETs
                    drop(guard);
                    orchestrator
                        .send_net_teardown(
                            client_ethernet,
                            client_docker,
                            server_ethernet,
                            server_docker,
                            net_id,
                        )
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
            services: Arc::new(RwLock::new(services)),
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
            net: (*NET_TYPE).into(),
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
