use crate::env::TIMEOUT;
use crate::orchestrator::Orchestrator;
use crate::services::changes::{apply_changes, detect_config_changes};
use crate::services::service_info::ServiceInfo;
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use nullnet_liberror::{Error, ErrorHandler, Location, location};
use serde::Deserialize;
use std::collections::HashMap;
use std::ops::Sub;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc as tokio_mpsc;
use tokio::sync::{Notify, RwLock};
use tokio::time::Instant;

const SERVICES_PATH: &str = "./services/services.toml";

#[derive(Deserialize)]
pub(crate) struct ServicesToml {
    services: Vec<ServiceToml>,
}

impl ServicesToml {
    pub(crate) async fn load() -> Result<HashMap<String, ServiceInfo>, Error> {
        Self::load_from(SERVICES_PATH).await
    }

    pub(crate) async fn load_from(path: &str) -> Result<HashMap<String, ServiceInfo>, Error> {
        let services_toml_str = tokio::fs::read_to_string(path)
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
        orchestrator: Orchestrator,
        config_changed: Arc<Notify>,
    ) -> Result<(), Error> {
        let mut services_directory = PathBuf::from(SERVICES_PATH);
        services_directory.pop();

        let (tx, mut rx) = tokio_mpsc::unbounded_channel();
        let mut watcher = RecommendedWatcher::new(
            move |event| {
                let _ = tx.send(event);
            },
            Config::default(),
        )
        .handle_err(location!())?;
        watcher
            .watch(&services_directory, RecursiveMode::Recursive)
            .handle_err(location!())?;

        let mut last_update_time = Instant::now().sub(Duration::from_secs(60));

        loop {
            let event = rx.recv().await;
            if event.is_none() {
                println!("File watcher channel closed, stopping watch");
                break;
            }
            if let Some(Ok(Event {
                kind: EventKind::Modify(_),
                ..
            })) = event
            {
                // debounce duplicated events
                if last_update_time.elapsed().as_millis() > 100 {
                    // ensure file changes are propagated
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    if let Ok(loaded_services) = ServicesToml::load().await {
                        let services_mut = &mut *services.write().await;
                        apply_config_update(services_mut, loaded_services, &orchestrator).await;
                        config_changed.notify_one();
                    }
                    last_update_time = Instant::now();
                }
            }
        }

        Ok(())
    }

    pub(crate) fn services_map(self) -> HashMap<String, ServiceInfo> {
        let mut ret_val: HashMap<String, ServiceInfo> = HashMap::new();

        // first insert proxy-unreachable services
        for s in &self.services {
            for d in &s.dependencies {
                // as dependencies of d, take dependencies of s after d in the list
                let d_deps = s
                    .dependencies
                    .iter()
                    .skip_while(|dep| *dep != d)
                    .skip(1)
                    .cloned()
                    .collect();
                ret_val.insert(d.clone(), ServiceInfo::new(d_deps, None));
            }
        }

        for s in self.services {
            ret_val.insert(
                s.name,
                ServiceInfo::new(s.dependencies, Some(s.timeout.unwrap_or(*TIMEOUT))),
            );
        }

        ret_val
    }
}

pub(crate) async fn apply_config_update(
    services: &mut HashMap<String, ServiceInfo>,
    loaded_services: HashMap<String, ServiceInfo>,
    orchestrator: &Orchestrator,
) {
    let changes = detect_config_changes(services, &loaded_services);
    apply_changes(changes, services, Some(&loaded_services), orchestrator).await;
}

#[derive(Deserialize)]
struct ServiceToml {
    name: String,
    /// Per-service proxy client timeout in seconds.
    /// If omitted, defaults to the global `TIMEOUT` env var (or 60s).
    /// A value of 0 disables the timeout (proxy clients never expire).
    timeout: Option<u64>,
    dependencies: Vec<String>,
}
