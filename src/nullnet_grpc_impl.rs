use crate::env::NET_TYPE;
use crate::graphviz::generate_graphviz;
use crate::orchestrator::Orchestrator;
use crate::proto::nullnet_grpc::nullnet_grpc_server::NullnetGrpc;
use crate::proto::nullnet_grpc::{
    BackendTriggerRequest, Empty, MsgId, NetMessage, NetType, ProxyRequest, Services, Upstream,
};
use crate::services::changes::{
    apply_changes, collect_dep_chain_edges, detect_services_list_changes,
};
use crate::services::clients::{Client, ClientInfo};
use crate::services::edge::RegisteredEdge;
use crate::services::input::ServicesToml;
use crate::services::service_info::ServiceInfo;
use crate::timeout::check_timeouts;
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
            if let Err(e) = ServicesToml::watch(&services_2, orchestrator_2, config_changed_2).await
            {
                eprintln!("failed to watch services.toml for changes: {e:?}");
            }
        });

        // periodically check for timed-out proxy clients and tear down their chains
        let services_2 = services.clone();
        let orchestrator_2 = orchestrator.clone();
        tokio::spawn(async move {
            check_timeouts(services_2, orchestrator_2, config_changed).await;
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

        let upstream = self
            .handle_proxy_request(&service_name, proxy_ip, &client_ip.to_string())
            .await?;
        Ok(Response::new(upstream))
    }

    pub(crate) async fn handle_proxy_request(
        &self,
        service_name: &str,
        proxy_ip: IpAddr,
        client_ip: &str,
    ) -> Result<Upstream, Error> {
        println!("Received proxy request for '{service_name}'");

        let service_info = self
            .services
            .read()
            .await
            .get(service_name)
            .cloned()
            .ok_or("Service not found")
            .handle_err(location!())?;

        if service_info.timeout().is_none() {
            Err("Service is not a configured entry point").handle_err(location!())?;
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
            if let Some(ServiceInfo::Registered(reg)) = services_mut.get_mut(service_name) {
                reg.set_latest_now(&proxy_client);
            }

            return Ok(upstream);
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
            if let Some(ServiceInfo::Registered(reg)) = services_mut.get_mut(service_name) {
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
                service_name,
                replica_ip,
                replica_docker.as_deref(),
                &services_mut,
            );
            for (dep_client, dep_name) in dep_edges {
                if let Some(ServiceInfo::Registered(dep_reg)) = services_mut.get_mut(&dep_name) {
                    dep_reg.add_chain(&dep_client);
                }
            }
            return Ok(upstream);
        }

        let response = self
            .new_proxy_chain(service_name, proxy_ip, client_ip)
            .await?;
        Ok(response.into_inner())
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
        let replica = reg
            .pick_replica_least_clients()
            .ok_or("Service has no replicas")
            .handle_err(location!())?;
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

    async fn build_dep_chain(
        &self,
        service_name: &str,
        service_ip: IpAddr,
        service_docker: Option<&str>,
    ) -> Result<Vec<RegisteredEdge>, Error> {
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

        dep_chain
            .into_iter()
            .map(|edge| {
                edge.into_registered()
                    .ok_or("Dependency not registered")
                    .handle_err(location!())
            })
            .collect::<Result<_, Error>>()
    }

    pub(crate) async fn setup_proxy_chain(
        &self,
        service_name: &str,
        proxy_ip: IpAddr,
        client_ip: &str,
        service_ip: IpAddr,
        service_docker: Option<&str>,
    ) -> Result<Ipv4Addr, Error> {
        let mut dep_chain = self
            .build_dep_chain(service_name, service_ip, service_docker)
            .await?;

        dep_chain.push(RegisteredEdge::new(
            proxy_ip,
            Client::new(client_ip.to_string(), Some(proxy_ip)),
            None,
            service_ip,
            Client::new(service_name.to_string(), None),
            service_docker.map(String::from),
        ));

        self.net_chain_setup(dep_chain)
            .await?
            .ok_or("No valid upstream IP found after NET chain setup")
            .handle_err(location!())
    }

    async fn backend_trigger_impl(
        &self,
        request: Request<BackendTriggerRequest>,
    ) -> Result<Response<Empty>, Error> {
        let sender_ip = request
            .remote_addr()
            .ok_or("Could not get remote address for backend trigger")
            .handle_err(location!())?
            .ip();

        let initiator_name = request.into_inner().service_name;
        self.handle_backend_trigger(&initiator_name, sender_ip)
            .await?;
        Ok(Response::new(Empty {}))
    }

    pub(crate) async fn handle_backend_trigger(
        &self,
        initiator_name: &str,
        sender_ip: IpAddr,
    ) -> Result<(), Error> {
        println!("Received backend trigger for '{initiator_name}' from {sender_ip}");

        let (initiator_ip, initiator_docker, first_dep_name) = {
            let guard = self.services.read().await;
            let si = guard
                .get(initiator_name)
                .ok_or("Initiator service not found")
                .handle_err(location!())?;
            if si.timeout().is_none() {
                Err("Initiator service is not a configured entry point").handle_err(location!())?;
            }
            let ServiceInfo::Registered(reg) = si else {
                Err("Initiator service is not registered").handle_err(location!())?
            };
            let replica = reg
                .replicas()
                .iter()
                .find(|r| r.ip() == sender_ip)
                .ok_or("No initiator replica found on sender host")
                .handle_err(location!())?;
            let first_dep = reg
                .dependencies()
                .first()
                .cloned()
                .ok_or("Initiator service has no dependencies")
                .handle_err(location!())?;
            (
                replica.ip(),
                replica.docker_container().map(String::from),
                first_dep,
            )
        };

        let initiator_client = Client::new_service(
            initiator_name.to_string(),
            initiator_ip,
            initiator_docker.clone(),
        );

        // Heartbeat: if the initiator is already set up on its first dep, just refresh `latest`.
        {
            let mut services_mut = self.services.write().await;
            if let Some(ServiceInfo::Registered(dep_reg)) = services_mut.get_mut(&first_dep_name)
                && dep_reg.is_client_setup(&initiator_client).is_some()
            {
                dep_reg.set_latest_now(&initiator_client);
                return Ok(());
            }
        }

        self.setup_backend_chain(initiator_name, initiator_ip, initiator_docker.as_deref())
            .await
    }

    pub(crate) async fn setup_backend_chain(
        &self,
        initiator_name: &str,
        initiator_ip: IpAddr,
        initiator_docker: Option<&str>,
    ) -> Result<(), Error> {
        let mut dep_chain = self
            .build_dep_chain(initiator_name, initiator_ip, initiator_docker)
            .await?;

        if let Some(first) = dep_chain.first_mut() {
            first.is_backend_entry = true;
        }

        self.net_chain_setup(dep_chain).await?;
        Ok(())
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
    ) -> Result<Option<Ipv4Addr>, Error> {
        let mut join_set_outer = JoinSet::new();
        for edge in dep_chain {
            let (client_ethernet, client) = edge.client;
            let (server_ethernet, server) = edge.server;
            let client_docker = edge.client_docker;
            let server_docker = edge.server_docker;
            let is_backend_entry = edge.is_backend_entry;

            let services = self.services.clone();
            let orchestrator = self.orchestrator.clone();
            join_set_outer.spawn(async move {
                let init_time = std::time::Instant::now();

                let mut services_guard = services.write().await;
                let Some(ServiceInfo::Registered(reg)) = services_guard.get_mut(server.name())
                else {
                    return EdgeOutcome::Failed;
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
                    return EdgeOutcome::Success {
                        client,
                        server_name: server.name().to_string(),
                        proxy_upstream: None,
                    };
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
                    return EdgeOutcome::Failed;
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
                    return EdgeOutcome::Failed;
                }

                let (Some(net_ip_server), Some(net_ip_client)) = (server_ok, client_ok) else {
                    return EdgeOutcome::Failed;
                };

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
                    )
                    .with_backend_entry(is_backend_entry);
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
                    return EdgeOutcome::Failed;
                }

                let proxy_upstream = if client.is_proxy().is_some() {
                    Some(net_ip_server)
                } else {
                    None
                };

                EdgeOutcome::Success {
                    client,
                    server_name: server.name().to_string(),
                    proxy_upstream,
                }
            });
        }

        let mut successful: Vec<SuccessfulEdge> = Vec::new();
        let mut any_failure = false;
        while let Some(res) = join_set_outer.join_next().await {
            match res {
                Ok(EdgeOutcome::Success {
                    client,
                    server_name,
                    proxy_upstream,
                }) => {
                    successful.push(SuccessfulEdge {
                        client,
                        server_name,
                        proxy_upstream,
                    });
                }
                Ok(EdgeOutcome::Failed) | Err(_) => {
                    any_failure = true;
                }
            }
        }

        if any_failure {
            let mut services_mut = self.services.write().await;
            for edge in &successful {
                if let Some(ServiceInfo::Registered(reg)) = services_mut.get_mut(&edge.server_name)
                {
                    reg.decrement_chain(&edge.client, &self.orchestrator).await;
                }
            }
            Err("NET chain setup failed").handle_err(location!())?;
        }

        let upstream = successful.iter().find_map(|e| e.proxy_upstream);
        Ok(upstream)
    }
}

enum EdgeOutcome {
    Success {
        client: Client,
        server_name: String,
        proxy_upstream: Option<Ipv4Addr>,
    },
    Failed,
}

struct SuccessfulEdge {
    client: Client,
    server_name: String,
    proxy_upstream: Option<Ipv4Addr>,
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

    async fn backend_trigger(
        &self,
        req: Request<BackendTriggerRequest>,
    ) -> Result<Response<Empty>, Status> {
        self.backend_trigger_impl(req)
            .await
            .map_err(|err| Status::internal(err.to_str()))
    }
}
