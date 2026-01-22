mod nullnet_grpc_impl;
mod orchestrator;
mod proto;

use crate::nullnet_grpc_impl::NullnetGrpcImpl;
use crate::proto::nullnet_grpc::nullnet_grpc_server::NullnetGrpcServer;
use nullnet_liberror::{Error, ErrorHandler, Location, location};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::{panic, process};
use tonic::transport::Server;

const PORT: u16 = 50051;

#[tokio::main]
async fn main() -> Result<(), Error> {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), PORT);

    let mut server = Server::builder();

    server
        .add_service(
            NullnetGrpcServer::new(init_nullnet()?).max_decoding_message_size(50 * 1024 * 1024),
        )
        .serve(addr)
        .await
        .handle_err(location!())?;

    Ok(())
}

fn init_nullnet() -> Result<NullnetGrpcImpl, Error> {
    if cfg!(not(debug_assertions)) {
        // custom panic hook to correctly clean up the server, even in case a secondary thread fails
        let orig_hook = panic::take_hook();
        panic::set_hook(Box::new(move |panic_info| {
            // invoke the default handler and exit the process
            orig_hook(panic_info);
            process::exit(1);
        }));
    }

    // handle termination signals: SIGINT, SIGTERM, SIGHUP
    ctrlc::set_handler(move || {
        process::exit(1);
    })
    .handle_err(location!())?;

    Ok(NullnetGrpcImpl::new())
}
