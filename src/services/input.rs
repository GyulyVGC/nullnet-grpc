use crate::env::TIMEOUT;
use crate::orchestrator::Orchestrator;
use crate::services::changes::{apply_changes, detect_config_changes};
use crate::services::service_info::ServiceInfo;
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use nullnet_liberror::{Error, ErrorHandler, Location, location};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
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
        let mut proxy_accum: HashMap<String, Vec<String>> = HashMap::new();
        // Trigger-chain deps: discoverable as (non-entry-point) services so
        // hosts can register replicas of them.
        let mut trigger_dep_names: HashSet<String> = HashSet::new();

        for s in &self.services {
            for d in &s.proxy_dependencies {
                proxy_accum.insert(d.clone(), tail_after(&s.proxy_dependencies, d));
            }
            for t in &s.triggers {
                for dep in &t.chain {
                    trigger_dep_names.insert(dep.clone());
                }
            }
        }

        let mut ret_val: HashMap<String, ServiceInfo> = HashMap::new();
        for (name, proxy) in proxy_accum {
            ret_val.insert(name, ServiceInfo::new(proxy, HashMap::new(), None, None));
        }
        for name in trigger_dep_names {
            ret_val
                .entry(name)
                .or_insert_with(|| ServiceInfo::new(Vec::new(), HashMap::new(), None, None));
        }

        // Explicit declarations override any implicit entries for the same
        // name and are treated as entry points (`Some(timeout)`). To register a
        // service as a backend dep without making it an entry point, do not
        // declare it explicitly — listing it in a `triggers.chain` is enough.
        for s in self.services {
            let triggers = s.triggers.into_iter().map(|t| (t.port, t.chain)).collect();
            ret_val.insert(
                s.name,
                ServiceInfo::new(
                    s.proxy_dependencies,
                    triggers,
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
    /// Per-service entry timeout in seconds for proxy clients. If omitted,
    /// defaults to the global `TIMEOUT` env var (or 60s). A value of 0
    /// disables the timeout. Any explicit declaration is treated as an entry
    /// point; backend deps without a proxy-reachable role should be left out
    /// of explicit declarations and picked up implicitly via trigger chains.
    timeout: Option<u64>,
    /// Linear dep chain walked on proxy-triggered setup.
    #[serde(default)]
    proxy_dependencies: Vec<String>,
    /// Backend-triggered chains: each entry pairs a port observed by the
    /// service host with the linear chain to bring up. One chain per port.
    #[serde(default)]
    triggers: Vec<TriggerToml>,
    /// Maximum number of networks that can be created for this service.
    /// Applies to proxy chains only (backend chains are unbounded).
    /// When the limit is reached, new proxy clients reuse an existing network
    /// on the same proxy node instead of creating a new one.
    max_networks: Option<u32>,
}

#[derive(Deserialize)]
struct TriggerToml {
    port: u16,
    #[serde(default)]
    chain: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_explicit_and_implicit_services() {
        let toml_str = r#"
[[services]]
name = "color.com"
timeout = 0
proxy_dependencies = ["pre.fs.color.com", "fs.color.com"]

[[services.triggers]]
port = 5555
chain = ["ts.color.com", "deeper.dep"]

[[services]]
name = "fs.color.com"
timeout = 30
"#;
        let parsed: ServicesToml = toml::from_str(toml_str).unwrap();
        let map = parsed.services_map();

        // explicit entry points keep their configured timeout
        assert_eq!(map["color.com"].timeout(), Some(0));
        assert_eq!(map["fs.color.com"].timeout(), Some(30));

        // every name referenced in a proxy_dependencies list or trigger chain
        // is implicitly added with timeout=None (registrable as a dep, not an
        // entry point), regardless of its position in the chain
        assert_eq!(map["pre.fs.color.com"].timeout(), None);
        assert_eq!(map["ts.color.com"].timeout(), None);
        assert_eq!(map["deeper.dep"].timeout(), None);
    }
}
