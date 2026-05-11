use crate::env::NET_TYPE;
use crate::services::clients::ClientInfo;
use crate::services::service_info::ServiceInfo;
use nullnet_liberror::{ErrorHandler, Location, location};
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

pub(crate) fn render_graphviz(services: &HashMap<String, ServiceInfo>) -> String {
    let mut entries: Vec<_> = services.iter().collect();
    entries.sort_by_key(|(name, _)| *name);

    // Replica identities that initiate an outgoing edge somewhere (proxy or
    // backend dep walk). Used so a replica counts as "active" even when its
    // own client map is empty — it's still doing work as a chain source.
    let initiators: HashSet<(String, IpAddr, Option<String>)> = services
        .values()
        .filter_map(|info| {
            if let ServiceInfo::Registered(reg) = info {
                Some(reg.replicas().iter().flat_map(|r| r.clients().keys()))
            } else {
                None
            }
        })
        .flatten()
        .filter_map(|c| {
            c.replica_identity()
                .map(|(ip, docker)| (c.name().to_string(), ip, docker.map(String::from)))
        })
        .collect();

    let mut graphviz = String::from(
        "digraph G {\n\
            \tbgcolor=grey10;\n\
            \tnode [color=white, fontcolor=white];\n\
            \tedge [color=white, fontcolor=white, fontsize=9, labelangle=180, labeldistance=0.8];\n\n",
    );
    for (name, info) in entries {
        let style = info.graphviz_style();
        let label = info.graphviz_label(name, &initiators);
        let _ =
            writeln!(graphviz, "\t\"{name}\" [label=\"{label}\"] {style};").handle_err(location!());
        if let ServiceInfo::Registered(registered) = info {
            let mut edges: Vec<_> = registered
                .replicas()
                .iter()
                .flat_map(|replica| replica.clients().iter())
                .collect();
            edges.sort_by_key(|(c, _)| c.display_name());
            for (c, ci) in edges {
                let c_name = c.display_name();
                let edge_label = ci.graphviz_edge_label(false);
                let _ = writeln!(graphviz, "\t\"{c_name}\" -> \"{name}\" {edge_label};")
                    .handle_err(location!());
            }
        }
        graphviz.push('\n');
    }
    graphviz = graphviz.trim().to_string();
    graphviz.push_str("\n}\n");
    graphviz
}

pub(crate) async fn generate_graphviz(services: Arc<RwLock<HashMap<String, ServiceInfo>>>) {
    loop {
        let services = services.read().await.clone();
        let graphviz = render_graphviz(&services);
        let _ = tokio::fs::write("graph.dot", graphviz)
            .await
            .handle_err(location!());

        println!("Regenerated graphviz");

        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

impl ServiceInfo {
    fn graphviz_label(
        &self,
        name: &str,
        initiators: &HashSet<(String, IpAddr, Option<String>)>,
    ) -> String {
        if let ServiceInfo::Registered(reg) = self {
            let total = reg.replicas().len();
            let active = reg
                .replicas()
                .iter()
                .filter(|r| {
                    !r.clients().is_empty()
                        || initiators.contains(&(
                            name.to_string(),
                            r.ip(),
                            r.docker_container().map(String::from),
                        ))
                })
                .count();
            return format!("{name} ({active}/{total})");
        }
        name.to_string()
    }

    fn graphviz_style(&self) -> &'static str {
        let is_entry_point = self.timeout().is_some();
        match self {
            ServiceInfo::Unregistered(_) if is_entry_point => "[style=solid, color=red]",
            ServiceInfo::Unregistered(_) => "[style=dashed, color=red]",
            ServiceInfo::Registered(_) if is_entry_point => "[style=solid, color=green]",
            ServiceInfo::Registered(_) => "[style=dashed, color=green]",
        }
    }
}

impl ClientInfo {
    fn graphviz_edge_label(&self, show_ends: bool) -> String {
        let client_br = self.client_net();
        let server_br = self.server_net();
        let net_id = self.net_id();
        let time_ms = self.time_ms();
        let net_type_str = NET_TYPE.as_str_name();
        if show_ends {
            format!(
                "[label=\"{net_type_str} {net_id} [{time_ms}ms]\", taillabel=\"{client_br}\", headlabel=\"{server_br}\"]"
            )
        } else {
            format!("[label=\"{net_type_str} {net_id} [{time_ms}ms]\"]")
        }
    }
}
