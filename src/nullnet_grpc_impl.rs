use crate::graphviz::generate_graphviz;
use crate::orchestrator::Orchestrator;
use crate::proto::nullnet_grpc::nullnet_grpc_server::NullnetGrpc;
use crate::proto::nullnet_grpc::{
    Empty, HostMapping, MsgId, ProxyRequest, Services, Upstream, VlanSetup,
};
use crate::services::clients::{Client, ClientInfo};
use crate::services::input::ServicesToml;
use crate::services::service_info::ServiceInfo;
use crate::vlan::cleanup_vlans_failed_service;
use nullnet_liberror::{Error, ErrorHandler, Location, location};
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock, mpsc};
use tokio::task::JoinSet;
use tonic::codegen::tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

pub(crate) struct NullnetGrpcImpl {
    /// The available services
    services: Arc<RwLock<HashMap<String, ServiceInfo>>>,
    /// Last registered VLAN ID
    last_registered_vlan: Arc<Mutex<u16>>,
    /// Orchestrator to manage TAP-based clients and VLAN setups
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

        // keep services up to date with the services.toml file
        let services_2 = services.clone();
        tokio::spawn(async move {
            ServicesToml::watch(&services_2)
                .await
                .expect("failed to watch services.toml for changes");
        });

        Ok(NullnetGrpcImpl {
            services,
            // start from next id = 2 since 0 is reserved and 1 is the default VLAN
            last_registered_vlan: Arc::new(Mutex::new(1)),
            orchestrator: Orchestrator::new(),
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

        let (service_ip, service_port) = registered.ip_port();

        // setup dependent services' VLANs
        let mut dep_chain =
            registered.dependency_chain(service_name.clone(), &*self.services.read().await)?;
        dep_chain.push((
            (proxy_ip, proxy_client),
            (service_ip, Client::new(service_name, None)),
        ));
        // create dedicated VLAN across all the client/server pair of the dependency chain
        let upstream_ip = self.vlan_chain_setup(dep_chain).await?;

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
            .filter(|name| !req.services.iter().any(|s| s.name == *name))
            .collect();

        let mut services_mut = self.services.write().await;

        // unregister services that are no longer present
        for service_name in to_be_unregistered {
            cleanup_vlans_failed_service(service_name.clone(), &mut services_mut).await?;
        }

        // re-register services that are still present
        for service in req.services {
            let service_port = u16::try_from(service.port).handle_err(location!())?;
            let service_name = service.name;
            services_mut.entry(service_name.clone()).and_modify(|si| {
                si.register(sender_ip, service_port);
            });
        }

        drop(services_mut);

        Ok(Response::new(Empty {}))
    }

    pub(crate) async fn vlan_chain_setup(
        &self,
        dep_chain: Vec<((IpAddr, Client), (IpAddr, Client))>,
    ) -> Result<IpAddr, Error> {
        let mut join_set_outer = JoinSet::new();
        for ((client_ethernet, client), (server_ethernet, server)) in dep_chain {
            let services = self.services.clone();
            let orchestrator = self.orchestrator.clone();
            let last_registered_vlan = self.last_registered_vlan.clone();
            join_set_outer.spawn(async move {
                let init_time = std::time::Instant::now();

                // add chain and check if the link is already set up
                let mut services_guard = services.write().await;
                let Some(ServiceInfo::Registered(reg)) = services_guard.get_mut(&server.name())
                else {
                    return None;
                };
                if reg.is_client_setup(&client).is_some() {
                    reg.add_chain(&client);
                    return None;
                }
                drop(services_guard);

                let mut last_id = last_registered_vlan.lock().await;
                *last_id += 1;
                let vlan_id = *last_id;
                drop(last_id);

                let [a, b] = vlan_id.to_be_bytes();
                let server_veth = IpAddr::V4(Ipv4Addr::new(10, a, b, 1));
                let client_veth = IpAddr::V4(Ipv4Addr::new(10, a, b, 2));
                let host_mapping = Some(HostMapping {
                    ip: server_veth.to_string(),
                    name: server.name(),
                });
                let msg = VlanSetup {
                    msg_id: None,
                    client_ethernet: client_ethernet.to_string(),
                    server_ethernet: server_ethernet.to_string(),
                    client_veth: client_veth.to_string(),
                    server_veth: server_veth.to_string(),
                    vlan_id: u32::from(vlan_id),
                    host_mapping,
                };

                let upstream_ip = if client.is_proxy() {
                    Some(server_veth)
                } else {
                    None
                };

                let destinations = [client_ethernet, server_ethernet];
                // remove duplicates from destinations (in case services are hosted on the same machine)
                let destinations: HashSet<IpAddr> = destinations.iter().copied().collect();

                let mut join_set_inner = JoinSet::new();
                for dest in destinations {
                    let msg = msg.clone();
                    let orchestrator = orchestrator.clone();
                    join_set_inner.spawn(async move {
                        // TODO: handle errors?
                        let _ = orchestrator.send_vlan_setup(dest, msg).await;
                        println!("{dest} acknowledged");
                    });
                }

                while join_set_inner.join_next().await.is_some() {}

                // register the link between the two services
                services
                    .write()
                    .await
                    .entry(server.name())
                    .and_modify(|si| {
                        if let ServiceInfo::Registered(reg) = si {
                            let time_ms = init_time.elapsed().as_millis();
                            let ci = ClientInfo::new(client_veth, server_veth, vlan_id, time_ms);
                            reg.add_client(client.clone(), ci);
                            reg.add_chain(&client);
                        }
                    });

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
            .ok_or("No valid upstream IP found after VLAN chain setup")
            .handle_err(location!())
    }
}

#[tonic::async_trait]
impl NullnetGrpc for NullnetGrpcImpl {
    type ControlChannelStream = ReceiverStream<Result<VlanSetup, Status>>;

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
