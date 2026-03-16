use crate::proto::nullnet_grpc::Net;

pub static NET_TYPE: std::sync::LazyLock<Net> = std::sync::LazyLock::new(|| {
    let str = std::env::var("NET_TYPE").unwrap_or_else(|_| {
        println!("'NET_TYPE' environment variable not set");
        String::new()
    });

    match str.to_uppercase().as_str() {
        "VXLAN" => Net::Vxlan,
        "VLAN" => Net::Vlan,
        _ => Net::default(),
    }
});

pub static TIMEOUT: std::sync::LazyLock<u64> = std::sync::LazyLock::new(|| {
    let str = std::env::var("TIMEOUT").unwrap_or_else(|_| {
        println!("'TIMEOUT' environment variable not set");
        String::new()
    });

    str.parse().unwrap_or(60)
});
