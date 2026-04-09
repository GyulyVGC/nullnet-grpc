# nullnet-grpc
gRPC-based controller for the Nullnet architecture

This repository contains the gRPC-based controller for the Nullnet architecture.

The controller is responsible for managing the network:
- receives hosted services from the clients part of the network
- receives external requests asking for a specific service
- creates on-the-fly the needed infrastructure (VLANs / VXLANs) to connect the client hosting the service to the external client asking for it
- destroys the infrastructure when the service is no longer needed
- keeps local configuration about the hosted services and their dependency chain

## Prerequisites

- install Rust
```
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

- install protobuf
```
curl -OL https://github.com/google/protobuf/releases/download/v3.20.3/protoc-3.20.3-linux-x86_64.zip
unzip protoc-3.20.3-linux-x86_64.zip -d protoc3
sudo mv protoc3/bin/* /usr/local/bin/
sudo mv protoc3/include/* /usr/local/include/
```

## Usage

### nullnet-grpc-server

- set environment variables
```
NET_TYPE=VXLAN
TIMEOUT=0
```

- service configuration must be stored at `./services/services.toml` and declare services as follows:
```
[[services]]
name = "color.com"
timeout = 0
dependencies = ["fs.color.com"]
max_networks = 3

[[services]]
...
```

- run the project:
```
cargo build --release
sudo ./target/release/nullnet-grpc-server
```

- the server will regularly update a view of the network and store it in the file `./graph.dot`

***

### nullnet-proxy-test

- set environment variables (set `CONTROL_SERVICE_ADDR` to the IP of nullnet-grpc-server)
```
CONTROL_SERVICE_ADDR=192.168.1.100
CONTROL_SERVICE_PORT=50051
```

- run the project:
```
cargo build --release
sudo ./target/release/nullnet-proxy-test
```

- the proxy will run on port 7777 and receive requests in the form `service_name:7777`

***

### tun

- set environment variables (set `CONTROL_SERVICE_ADDR` to the IP of nullnet-grpc-server)
```
CONTROL_SERVICE_ADDR=192.168.1.100
CONTROL_SERVICE_PORT=50051
```

- service configuration must be stored at `./services.toml` and declare services as follows:
```
# services = [] # use this if you don't want to declare any service

[[services]]
name = "color.com"
port = 3001
docker_container = "stack-name_container-name" # should correspond to the label "com.docker.swarm.service.name"

[[services]]
...
```

- run the project:
```
cargo build --release
sudo ./target/release/tun
```
