use crate::clients::ClientInfo;
use crate::orchestrator::Orchestrator;
use crate::proto::nullnet_grpc::nullnet_grpc_server::NullnetGrpc;
use crate::proto::nullnet_grpc::{Empty, HostMapping, ProxyRequest, Services, Upstream, VlanSetup};
use crate::service_info::{ServiceInfo, ServicesToml};
use nullnet_liberror::{Error, ErrorHandler, Location, location};
use std::collections::HashMap;
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

        if let Some(upstream) = registered.is_proxy_client_setup(client_ip) {
            println!("'{client_ip}' ---> '{service_name}' is already set up");
            return Ok(Response::new(upstream));
        }

        // setup dependent services' VLANs
        for ((h1, h1_name), (h2, h2_name)) in registered
            .dependency_chain(service_name.clone(), &self.services)
            .await?
        {
            let init_time = std::time::Instant::now();

            // check if the link is already set up
            let h2_service = self.services.read().await.get(&h2_name).cloned();
            if let Some(ServiceInfo::Registered(reg)) = h2_service
                && reg.is_service_client_setup(&h1_name)
            {
                continue;
            }

            let vlan_id = self.next_vlan_id().await;
            let [a, b] = vlan_id.to_be_bytes();

            let destinations = vec![h2, h1];

            // create dedicated VLAN on the machine where the "server" service is running on
            let server_veth = IpAddr::V4(Ipv4Addr::new(10, a, b, 1));
            self.orchestrator
                .send_vlan_setup_requests(h2, server_veth, vlan_id, &destinations, None)
                .await?;

            // create dedicated VLAN on the machine where the "client" service is running on
            // also register the "server" service on the "client" service machine's hosts file
            let client_veth = IpAddr::V4(Ipv4Addr::new(10, a, b, 2));
            let host_mapping = HostMapping {
                ip: server_veth.to_string(),
                name: h2_name.clone(),
            };
            self.orchestrator
                .send_vlan_setup_requests(
                    h1,
                    client_veth,
                    vlan_id,
                    &destinations,
                    Some(host_mapping),
                )
                .await?;

            let time_ms = init_time.elapsed().as_millis();

            // register the link between the two services
            self.services.write().await.entry(h2_name).and_modify(|si| {
                if let ServiceInfo::Registered(reg) = si {
                    let ci = ClientInfo::new(client_veth, server_veth, vlan_id, time_ms);
                    reg.add_service_client(h1_name, ci);
                }
            });
        }

        let init_time = std::time::Instant::now();

        let (service_ip, service_port) = registered.ip_port();

        let vlan_id = self.next_vlan_id().await;
        let [a, b] = vlan_id.to_be_bytes();

        let destinations = vec![service_ip, proxy_ip];

        // create dedicated VLAN on the machine where the "server" service is running on
        let server_veth = IpAddr::V4(Ipv4Addr::new(10, a, b, 1));
        self.orchestrator
            .send_vlan_setup_requests(service_ip, server_veth, vlan_id, &destinations, None)
            .await?;

        // create dedicated VLAN on the machine where the "client" proxy is running on
        let client_veth = IpAddr::V4(Ipv4Addr::new(10, a, b, 2));
        self.orchestrator
            .send_vlan_setup_requests(proxy_ip, client_veth, vlan_id, &destinations, None)
            .await?;

        let time_ms = init_time.elapsed().as_millis();

        // register the link between the service and the proxy client
        self.services
            .write()
            .await
            .entry(service_name)
            .and_modify(|si| {
                if let ServiceInfo::Registered(reg) = si {
                    let ci = ClientInfo::new(client_veth, server_veth, vlan_id, time_ms);
                    reg.add_proxy_client(client_ip, ci);
                }
            });

        // regenerate the service graphviz for debugging
        let _ = self.generate_graphviz().await;

        Ok(Response::new(Upstream {
            ip: server_veth.to_string(),
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
                for (c, ci) in registered.all_clients() {
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
