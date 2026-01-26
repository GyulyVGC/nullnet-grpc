use std::net::IpAddr;

#[derive(Clone)]
pub(crate) struct ServiceInfo {
    pub(crate) ip: IpAddr,
    pub(crate) port: u16,
    pub(crate) dependencies: Vec<ServiceDependency>,
}

#[derive(Clone)]
pub(crate) struct ServiceDependency {
    pub(crate) name: String,
    pub(crate) is_setup: bool,
}
