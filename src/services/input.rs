use crate::services::service_info::ServiceInfo;
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use nullnet_liberror::{Error, ErrorHandler, Location, location};
use serde::Deserialize;
use std::collections::HashMap;
use std::ops::Sub;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::Instant;

const SERVICES_PATH: &str = "./services/services.toml";

#[derive(Deserialize)]
pub(crate) struct ServicesToml {
    services: Vec<ServiceToml>,
}

impl ServicesToml {
    pub(crate) async fn load() -> Result<HashMap<String, ServiceInfo>, Error> {
        // read services from file
        let services_toml_str = tokio::fs::read_to_string(SERVICES_PATH)
            .await
            .handle_err(location!())?;
        let services_toml: ServicesToml =
            toml::from_str(&services_toml_str).handle_err(location!())?;
        let services = services_toml.services_map();
        println!("Loaded services: {services:?}");
        Ok(services)
    }

    pub(crate) async fn watch(
        services: &Arc<RwLock<HashMap<String, ServiceInfo>>>,
    ) -> Result<(), Error> {
        let mut services_directory = PathBuf::from(SERVICES_PATH);
        services_directory.pop();

        let (tx, rx) = std::sync::mpsc::channel();
        let mut watcher = RecommendedWatcher::new(tx, Config::default()).handle_err(location!())?;
        watcher
            .watch(&services_directory, RecursiveMode::Recursive)
            .handle_err(location!())?;

        let mut last_update_time = Instant::now().sub(Duration::from_secs(60));

        loop {
            // only update services if the event is related to a file change
            if let Ok(Ok(Event {
                kind: EventKind::Modify(_),
                ..
            })) = rx.recv()
            {
                // debounce duplicated events
                if last_update_time.elapsed().as_millis() > 100 {
                    // ensure file changes are propagated
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    if let Ok(loaded_services) = ServicesToml::load().await {
                        *services.write().await = loaded_services;
                    }
                    last_update_time = Instant::now();
                }
            }
        }
    }

    fn services_map(self) -> HashMap<String, ServiceInfo> {
        let mut ret_val: HashMap<String, ServiceInfo> = HashMap::new();

        // first insert proxy-unreachable services
        for s in &self.services {
            for d in &s.dependencies {
                ret_val.insert(d.clone(), ServiceInfo::new(Vec::new(), false));
            }
        }

        for s in self.services {
            ret_val.insert(s.name, ServiceInfo::new(s.dependencies, true));
        }

        ret_val
    }
}

#[derive(Deserialize)]
struct ServiceToml {
    name: String,
    dependencies: Vec<String>,
}
