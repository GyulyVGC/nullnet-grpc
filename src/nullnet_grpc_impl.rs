use crate::clients::ClientInfo;
use crate::orchestrator::Orchestrator;
use crate::proto::nullnet_grpc::nullnet_grpc_server::NullnetGrpc;
use crate::proto::nullnet_grpc::{Empty, HostMapping, ProxyRequest, Services, Upstream, VlanSetup};
use crate::service_info::{ServiceInfo, ServicesToml};
use nullnet_liberror::{Error, ErrorHandler, Location, location};
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock, mpsc};
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
        // read services from file
        let services_toml_str = tokio::fs::read_to_string("services.toml")
            .await
            .handle_err(location!())?;
        let services_toml: ServicesToml =
            toml::from_str(&services_toml_str).handle_err(location!())?;
        println!("Loaded services: {services_toml:?}");

        let ret = NullnetGrpcImpl {
            services: Arc::new(RwLock::new(services_toml.services_map())),
            // start from next id = 2 since 0 is reserved and 1 is the default VLAN
            last_registered_vlan: Arc::new(Mutex::new(1)),
            orchestrator: Orchestrator::new(),
        };

        // regenerate the service graphviz for debugging
        let _ = ret.generate_graphviz().await;

        Ok(ret)
    }

    async fn control_channel_impl(
        &self,
        request: Request<Streaming<Empty>>,
    ) -> Result<Response<<NullnetGrpcImpl as NullnetGrpc>::ControlChannelStream>, Error> {
        let (sender, receiver) = mpsc::channel(64);

        let sender_ip = request
            .remote_addr()
            .ok_or("Could not get remote address for control channel request")
            .handle_err(location!())?
            .ip();
        self.orchestrator
            .add_client(sender_ip, request.into_inner(), sender)
            .await;

        Ok(Response::new(ReceiverStream::new(receiver)))
    }

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

        if let Some(upstream) = registered.is_client_setup(&client_ip.to_string()) {
            println!("'{client_ip}' ---> '{service_name}' is already set up");
            return Ok(Response::new(upstream));
        }

        let (service_ip, service_port) = registered.ip_port();

        // setup dependent services' VLANs
        let mut dep_chain = registered
            .dependency_chain(service_name.clone(), &self.services)
            .await?;
        dep_chain.push((
            (proxy_ip, client_ip.to_string()),
            (service_ip, service_name.clone()),
        ));
        // create dedicated VLAN across all the client/server pair of the dependency chain
        let upstream_ip = self.vlan_chain_setup(dep_chain).await?;

        // regenerate the service graphviz for debugging
        let _ = self.generate_graphviz().await;

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
            services_mut.entry(service_name).and_modify(|si| {
                // re-check that it's still registered from this sender_ip to avoid race conditions
                if let ServiceInfo::Registered(reg) = si {
                    let (ip, _) = reg.ip_port();
                    if ip == sender_ip {
                        si.unregister();
                    }
                }
            });
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

        // regenerate the service graphviz for debugging
        let _ = self.generate_graphviz().await;

        Ok(Response::new(Empty {}))
    }

    pub(crate) async fn vlan_chain_setup(
        &self,
        dep_chain: Vec<((IpAddr, String), (IpAddr, String))>,
    ) -> Result<IpAddr, Error> {
        let mut upstream_ip = IpAddr::V4(Ipv4Addr::UNSPECIFIED);

        for ((client_eth, client_name), (server_eth, server_name)) in dep_chain {
            let init_time = std::time::Instant::now();

            // check if the link is already set up
            let server_service = self.services.read().await.get(&server_name).cloned();
            if let Some(ServiceInfo::Registered(reg)) = server_service
                && reg.is_client_setup(&client_name).is_some()
            {
                continue;
            }

            let vlan_id = self.next_vlan_id().await;

            let destinations = [client_eth, server_eth];
            // remove duplicates from destinations (in case services are hosted on the same machine)
            let destinations: HashSet<IpAddr> = destinations.iter().copied().collect();

            let [a, b] = vlan_id.to_be_bytes();
            let server_veth = IpAddr::V4(Ipv4Addr::new(10, a, b, 1));
            let client_veth = IpAddr::V4(Ipv4Addr::new(10, a, b, 2));
            upstream_ip = server_veth;

            let host_mapping = Some(HostMapping {
                ip: server_veth.to_string(),
                name: server_name.clone(),
            });

            let msg = VlanSetup {
                client_eth: client_eth.to_string(),
                server_eth: server_eth.to_string(),
                client_veth: client_veth.to_string(),
                server_veth: server_veth.to_string(),
                vlan_id: u32::from(vlan_id),
                host_mapping,
            };

            let clients = self.orchestrator.clients.read().await;
            for dest in &destinations {
                let Some(mutex) = clients.get(dest) else {
                    continue;
                };
                let (inbound, outbound) = &mut *mutex.lock().await;

                outbound
                    .send(Ok(msg.clone()))
                    .await
                    .handle_err(location!())?;

                let _ = inbound.message().await;

                println!("{dest} acknowledged");
            }
            drop(clients);

            // register the link between the two services
            self.services
                .write()
                .await
                .entry(server_name)
                .and_modify(|si| {
                    if let ServiceInfo::Registered(reg) = si {
                        let time_ms = init_time.elapsed().as_millis();
                        let ci = ClientInfo::new(client_veth, server_veth, vlan_id, time_ms);
                        reg.add_client(client_name, ci);
                    }
                });
        }

        Ok(upstream_ip)
    }

    async fn next_vlan_id(&self) -> u16 {
        let mut last_id = self.last_registered_vlan.lock().await;
        *last_id += 1;
        *last_id
    }

    async fn generate_graphviz(&self) -> Result<(), Error> {
        let services = self.services.read().await.clone();
        let mut graphviz = String::from(
            "digraph G {\n\
                \tbgcolor=grey10;\n\
                \tnode [color=white, fontcolor=white];\n\
                \tedge [color=white, fontcolor=white, fontsize=9, labelangle=180, labeldistance=0.8];\n\n",
        );
        for (name, info) in services {
            let style = info.graphviz_style();
            writeln!(graphviz, "\t\"{name}\" {style};").handle_err(location!())?;
            if let ServiceInfo::Registered(registered) = info {
                for (c, ci) in registered.clients() {
                    let edge_label = ci.graphviz_edge_label(false);
                    writeln!(graphviz, "\t\"{c}\" -> \"{name}\" {edge_label};")
                        .handle_err(location!())?;
                }
            }
            graphviz.push('\n');
        }
        graphviz = graphviz.trim().to_string();
        graphviz.push_str("\n}\n");
        tokio::fs::write("graph.dot", graphviz)
            .await
            .handle_err(location!())?;

        Ok(())
    }
}

#[tonic::async_trait]
impl NullnetGrpc for NullnetGrpcImpl {
    type ControlChannelStream = ReceiverStream<Result<VlanSetup, Status>>;

    async fn control_channel(
        &self,
        request: Request<Streaming<Empty>>,
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
