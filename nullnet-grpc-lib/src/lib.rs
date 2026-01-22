mod proto;

use crate::nullnet_grpc::nullnet_grpc_client::NullnetGrpcClient;
use crate::nullnet_grpc::{Empty, ProxyRequest, Services, Upstream, VlanSetup};
pub use proto::*;
use tokio::sync::mpsc;
use tonic::Request;
pub use tonic::Streaming;
use tonic::codegen::tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Channel, ClientTlsConfig};

#[derive(Clone)]
pub struct NullnetGrpcInterface {
    client: NullnetGrpcClient<Channel>,
}

impl NullnetGrpcInterface {
    #[allow(clippy::missing_errors_doc)]
    pub async fn new(host: &str, port: u16, tls: bool) -> Result<Self, String> {
        let protocol = if tls { "https" } else { "http" };

        let mut endpoint = Channel::from_shared(format!("{protocol}://{host}:{port}"))
            .map_err(|e| e.to_string())?
            .connect_timeout(std::time::Duration::from_secs(10));

        if tls {
            endpoint = endpoint
                .tls_config(ClientTlsConfig::new().with_native_roots())
                .map_err(|e| e.to_string())?;
        }

        let channel = endpoint.connect().await.map_err(|e| e.to_string())?;

        Ok(Self {
            client: NullnetGrpcClient::new(channel),
        })
    }

    #[allow(clippy::missing_errors_doc)]
    pub async fn control_channel(
        &self,
        receiver: mpsc::Receiver<Empty>,
    ) -> Result<Streaming<VlanSetup>, String> {
        let receiver = ReceiverStream::new(receiver);

        Ok(self
            .client
            .clone()
            .control_channel(Request::new(receiver))
            .await
            .map_err(|e| e.to_string())?
            .into_inner())
    }

    #[allow(clippy::missing_errors_doc)]
    pub async fn proxy(&self, message: ProxyRequest) -> Result<Upstream, String> {
        self.client
            .clone()
            .proxy(Request::new(message))
            .await
            .map(tonic::Response::into_inner)
            .map_err(|e| e.to_string())
    }

    #[allow(clippy::missing_errors_doc)]
    pub async fn services_list(&self, message: Services) -> Result<(), String> {
        self.client
            .clone()
            .services_list(Request::new(message))
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}
