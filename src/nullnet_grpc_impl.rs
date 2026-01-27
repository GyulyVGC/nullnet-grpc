use crate::orchestrator::Orchestrator;
use crate::proto::nullnet_grpc::nullnet_grpc_server::NullnetGrpc;
use crate::proto::nullnet_grpc::{Empty, HostMapping, ProxyRequest, Services, Upstream, VlanSetup};
use crate::service_info::{DependencyInfo, ServiceInfo, ServicesToml};
use nullnet_liberror::{Error, ErrorHandler, Location, location};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock, mpsc};
use tonic::codegen::tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

pub(crate) struct NullnetGrpcImpl {
    /// The available services
    services: Arc<RwLock<HashMap<String, ServiceInfo>>>,
    /// The dependencies of the services
    dependencies: Arc<RwLock<HashMap<String, DependencyInfo>>>,
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

        let services = services_toml.services_map();
        let dependencies = services_toml.dependencies_map();

        Ok(NullnetGrpcImpl {
            services: Arc::new(RwLock::new(services)),
            dependencies: Arc::new(RwLock::new(dependencies)),
            last_registered_vlan: Arc::new(Mutex::new(100)),
            orchestrator: Orchestrator::new(),
        })
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

        let Some(registered) = service_info.as_registered() else {
            Err("Service is not registered").handle_err(location!())?
        };

        if let Some(upstream) = registered.is_client_setup(client_ip) {
            println!("'{client_ip}' ---> '{service_name}' is already set up");
            return Ok(Response::new(upstream));
        }

        // setup dependent services' VLANs
        if !registered.are_dependencies_setup() {
            for (h1, (h2, dep_name)) in registered.dependency_chain(&self.dependencies).await? {
                let vlan_id = self.next_vlan_id().await;
                let [a, b] = vlan_id.to_be_bytes();

                let destinations = vec![h2, h1];

                // create dedicated VLAN on the machine where the dependent service is running on
                let dep_veth_ip = IpAddr::V4(Ipv4Addr::new(10, a, b, 1));
                self.orchestrator
                    .send_vlan_setup_requests(h2, dep_veth_ip, vlan_id, &destinations, None)
                    .await?;

                // create dedicated VLAN on the machine where the parent service is running on
                // also register the dependent service on the main service machine's hosts file
                let veth_ip = IpAddr::V4(Ipv4Addr::new(10, a, b, 2));
                let host_mapping = HostMapping {
                    ip: dep_veth_ip.to_string(),
                    name: dep_name.clone(),
                };
                self.orchestrator
                    .send_vlan_setup_requests(
                        h1,
                        veth_ip,
                        vlan_id,
                        &destinations,
                        Some(host_mapping),
                    )
                    .await?;
            }
        }

        let (service_ip, service_port) = registered.ip_port();

        let vlan_id = self.next_vlan_id().await;
        let [a, b] = vlan_id.to_be_bytes();

        let destinations = vec![service_ip, proxy_ip];

        // create dedicated VLAN on the machine where the service is running on
        let target_veth_ip = IpAddr::V4(Ipv4Addr::new(10, a, b, 1));
        self.orchestrator
            .send_vlan_setup_requests(service_ip, target_veth_ip, vlan_id, &destinations, None)
            .await?;

        // create dedicated VLAN on the machine where the proxy is running on
        let veth_ip = IpAddr::V4(Ipv4Addr::new(10, a, b, 2));
        self.orchestrator
            .send_vlan_setup_requests(proxy_ip, veth_ip, vlan_id, &destinations, None)
            .await?;

        // register the client IP to veth IP mapping
        self.services
            .write()
            .await
            .entry(service_name)
            .and_modify(|si| {
                if let ServiceInfo::Registered(reg) = si {
                    reg.setup_client(client_ip, target_veth_ip);
                }
            });

        Ok(Response::new(Upstream {
            ip: target_veth_ip.to_string(),
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

        for service in req.services {
            let service_port = u16::try_from(service.port).handle_err(location!())?;
            let service_name = service.name;
            self.services
                .write()
                .await
                .entry(service_name.clone())
                .and_modify(|si| {
                    *si = si.clone().register(sender_ip, service_port);
                });

            self.dependencies
                .write()
                .await
                .entry(service_name)
                .and_modify(|di| {
                    *di = DependencyInfo::registered(sender_ip);
                });
        }

        Ok(Response::new(Empty {}))
    }

    async fn next_vlan_id(&self) -> u16 {
        let mut last_id = self.last_registered_vlan.lock().await;
        *last_id += 1;
        *last_id
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
