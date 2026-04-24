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

Repositories of the different components part of the architecture:
- [nullnet-server](https://github.com/NullNet-ai/nullnet-server)
- [nullnet-proxy](https://github.com/NullNet-ai/nullnet-proxy)
- [nullnet-client](https://github.com/NullNet-ai/nullnet-client)

Each of them needs to be cloned under the `/root` directory to correctly run as daemon with the provided `linux-setup.sh` scripts.

### nullnet-server

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
proxy_dependencies = ["fs.color.com"]
backend_dependencies = [
  ["a.color.com", "b.color.com"],
  ["c.color.com"],
]
max_networks = 3

[[services]]
...
```

- `proxy_dependencies` is a linear dep chain walked when the service is reached via a `Proxy` RPC from nullnet-proxy
- `backend_dependencies` is a list of parallel linear chains walked when the service is reached via a `BackendTrigger` RPC from nullnet-client; fan-out = outer length

- run the project as daemon with:
```
./linux-setup.sh
```

- the server will regularly update a view of the network and store it in the file `./graph.dot`

***

### nullnet-proxy

- set environment variables (set `CONTROL_SERVICE_ADDR` to the IP of nullnet-grpc-server)
```
CONTROL_SERVICE_ADDR=192.168.1.100
CONTROL_SERVICE_PORT=50051
```

- run the project as daemon with:
```
./linux-setup.sh
```

- the proxy will run on port 7777 and receive requests in the form `service_name:7777`

***

### nullnet-client

- set environment variables (set `CONTROL_SERVICE_ADDR` to the IP of nullnet-grpc-server, `ETH_NAME` to the ethernet interface to monitor)
```
CONTROL_SERVICE_ADDR=192.168.1.100
CONTROL_SERVICE_PORT=50051
ETH_NAME=ens18
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

- trigger configuration must be stored at `./triggers.toml` and declare per-port triggers as follows:
```
# triggers = [] # use this if you don't want to declare any trigger

[[triggers]]
port = 8080
service_name = "color.com"

[[triggers]]
...
```

  When eBPF observes an outgoing packet on `ETH_NAME` whose destination port matches one of the declared triggers, the client sends a `BackendTrigger` RPC to the server with the corresponding `service_name`.

- run the project as daemon with:
```
./linux-setup.sh
```
