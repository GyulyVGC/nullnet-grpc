const NULLNET_GRPC_PATH: &str = "./proto/nullnet_grpc.proto";
const PROTOBUF_DIR_PATH: &str = "./proto";

fn main() {
    for out_dir in ["./src/proto", "./nullnet-grpc-lib/src/proto"] {
        tonic_prost_build::configure()
            .out_dir(out_dir)
            // .type_attribute("appguard.AppGuardIpInfo", "#[derive(serde::Deserialize)]")
            // .type_attribute(
            //     "appguard_commands.FirewallDefaults",
            //     "#[derive(serde::Deserialize)]",
            // )
            // .type_attribute(
            //     "appguard.Log",
            //     "#[derive(serde::Serialize, serde::Deserialize)]",
            // )
            .compile_protos(&[NULLNET_GRPC_PATH], &[PROTOBUF_DIR_PATH])
            .expect("Protobuf files generation failed");
    }
}
