use crate::orchestrator::Orchestrator;
use crate::proto::nullnet_grpc::nullnet_grpc_server::NullnetGrpc;
use crate::proto::nullnet_grpc::{Empty, ProxyRequest, Services, Upstream, VlanSetup};
use nullnet_liberror::{Error, ErrorHandler, Location, location};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock, mpsc};
use tonic::codegen::tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

pub(crate) struct NullnetGrpcImpl {
    /// The available services and their host machine addresses
    services: Arc<RwLock<HashMap<String, SocketAddr>>>,
    /// Last registered VLAN ID
    last_registered_vlan: Arc<Mutex<u16>>,
    /// Orchestrator to manage TAP-based clients and VLAN setups
    orchestrator: Orchestrator,
}

impl NullnetGrpcImpl {
    pub fn new() -> Self {
        NullnetGrpcImpl {
            services: Arc::new(RwLock::new(HashMap::new())),
            last_registered_vlan: Arc::new(Mutex::new(100)),
            orchestrator: Orchestrator::new(),
        }
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
        let service_socket = self
            .services
            .read()
            .await
            .get(&req.service_name)
            .copied()
            .ok_or("Service not found")
            .handle_err(location!())?;
        let service_ip = service_socket.ip();
        let service_port = service_socket.port();

        let vlan_id = {
            let mut last_id = self.last_registered_vlan.lock().await;
            *last_id += 1;
            *last_id
        };
        let [a, b] = vlan_id.to_be_bytes();

        let destinations = vec![proxy_ip, service_ip];

        // create dedicated VLAN on the machine where the proxy is running on
        let veth_ip = IpAddr::V4(Ipv4Addr::new(10, a, b, 1));
        self.orchestrator
            .send_vlan_setup_requests(proxy_ip, veth_ip, vlan_id, &destinations)
            .await?;

        // create dedicated VLAN on the machine where the service is running on
        let veth_ip = IpAddr::V4(Ipv4Addr::new(10, a, b, 2));
        self.orchestrator
            .send_vlan_setup_requests(service_ip, veth_ip, vlan_id, &destinations)
            .await?;

        Ok(Response::new(Upstream {
            ip: veth_ip.to_string(),
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
        for service in req.services {
            let service_port = u16::try_from(service.port).handle_err(location!())?;
            let service_name = service.name;
            self.services
                .write()
                .await
                .insert(service_name, SocketAddr::new(sender_ip, service_port));
        }

        Ok(Response::new(Empty {}))
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
                .map_or("unknown".into(), |addr| addr.to_string())
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
