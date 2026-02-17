use crate::services::service_info::ServiceInfo;
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Deserialize, Debug)]
pub(crate) struct ServicesToml {
    services: Vec<ServiceToml>,
}

impl ServicesToml {
    pub(crate) fn services_map(&self) -> HashMap<String, ServiceInfo> {
        let mut ret_val: HashMap<String, ServiceInfo> = HashMap::new();

        // first insert proxy-reachable services
        for s in &self.services {
            ret_val.insert(
                s.name.clone(),
                ServiceInfo::new(s.dependencies.clone(), true),
            );
        }

        for s in &self.services {
            for d in &s.dependencies {
                if !ret_val.contains_key(d) {
                    ret_val.insert(d.clone(), ServiceInfo::new(Vec::new(), false));
                }
            }
        }

        ret_val
    }
}

#[derive(Deserialize, Debug)]
pub(crate) struct ServiceToml {
    name: String,
    dependencies: Vec<String>,
}
