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

        let mut last_update_time = Instant::now().sub(Duration::from_mins(1));

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
        // Proxy: last-write-wins per dep (a name referenced from multiple
        // services has its tail overwritten by the last processor).
        // Backend: each inner chain's tail is appended so a dep shared across
        // chains inherits all their sub-tails as separate fan-out entries.
        let mut proxy_accum: HashMap<String, Vec<String>> = HashMap::new();
        let mut backend_accum: HashMap<String, Vec<Vec<String>>> = HashMap::new();

        for s in &self.services {
            for d in &s.proxy_dependencies {
                proxy_accum.insert(d.clone(), tail_after(&s.proxy_dependencies, d));
            }
            for chain in &s.backend_dependencies {
                for d in chain {
                    let tail = tail_after(chain, d);
                    let slot = backend_accum.entry(d.clone()).or_default();
                    if !tail.is_empty() {
                        slot.push(tail);
                    }
                }
            }
        }

        let mut ret_val: HashMap<String, ServiceInfo> = HashMap::new();
        for (name, proxy) in proxy_accum {
            let backend = backend_accum.remove(&name).unwrap_or_default();
            ret_val.insert(name, ServiceInfo::new(proxy, backend, None, None));
        }
        for (name, backend) in backend_accum {
            ret_val.insert(name, ServiceInfo::new(Vec::new(), backend, None, None));
        }

        // Explicit declarations override any implicit entries for the same name.
        for s in self.services {
            ret_val.insert(
                s.name,
                ServiceInfo::new(
                    s.proxy_dependencies,
                    s.backend_dependencies,
                    Some(s.timeout.unwrap_or(*TIMEOUT)),
                    s.max_networks,
                ),
            );
        }

        ret_val
    }
}

/// Return the elements of `slice` that come after the first occurrence of `elem`.
fn tail_after(slice: &[String], elem: &str) -> Vec<String> {
    slice
        .iter()
        .position(|d| d == elem)
        .map(|i| slice[i + 1..].to_vec())
        .unwrap_or_default()
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
    /// Per-service entry timeout in seconds, applied to both proxy clients and
    /// backend-triggered chain entries. If omitted, defaults to the global
    /// `TIMEOUT` env var (or 60s). A value of 0 disables the timeout.
    timeout: Option<u64>,
    /// Linear dep chain walked on proxy-triggered setup.
    #[serde(default)]
    proxy_dependencies: Vec<String>,
    /// Parallel dep chains walked on backend-triggered setup.
    /// Each inner array is one linear chain; fan-out = outer length.
    #[serde(default)]
    backend_dependencies: Vec<Vec<String>>,
    /// Maximum number of networks that can be created for this service.
    /// Applies to proxy chains only (backend chains are unbounded).
    /// When the limit is reached, new proxy clients reuse an existing network
    /// on the same proxy node instead of creating a new one.
    max_networks: Option<u32>,
}
