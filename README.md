# nullnet-grpc
gRPC-based controller for the Nullnet architecture

This repository contains the gRPC-based controller for the Nullnet architecture.

The controller is responsible for managing the network:
- receives hosted services from the clients part of the network
- receives external requests asking for a specific service
- creates on-the-fly the needed infrastructure (VLANs / VXLANs) to connect the client hosting the service to the external client asking for it
- destroys the infrastructure when the service is no longer needed
- keeps local configuration about the hosted services and their dependency chain
