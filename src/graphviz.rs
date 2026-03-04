use crate::services::clients::ClientInfo;
use crate::services::service_info::ServiceInfo;
use nullnet_liberror::{ErrorHandler, Location, location};
use std::collections::HashMap;
use std::fmt::Write;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

pub(crate) async fn generate_graphviz(services: Arc<RwLock<HashMap<String, ServiceInfo>>>) {
    loop {
        let services = services.read().await.clone();
        let mut graphviz = String::from(
            "digraph G {\n\
                \tbgcolor=grey10;\n\
                \tnode [color=white, fontcolor=white];\n\
                \tedge [color=white, fontcolor=white, fontsize=9, labelangle=180, labeldistance=0.8];\n\n",
        );
        for (name, info) in services {
            let style = info.graphviz_style();
            let _ = writeln!(graphviz, "\t\"{name}\" {style};").handle_err(location!());
            if let ServiceInfo::Registered(registered) = info {
                for (c, ci) in registered.clients() {
                    let c_name = c.name();
                    let edge_label = ci.graphviz_edge_label(false);
                    let _ = writeln!(graphviz, "\t\"{c_name}\" -> \"{name}\" {edge_label};")
                        .handle_err(location!());
                }
            }
            graphviz.push('\n');
        }
        graphviz = graphviz.trim().to_string();
        graphviz.push_str("\n}\n");
        let _ = tokio::fs::write("graph.dot", graphviz)
            .await
            .handle_err(location!());

        println!("Regenerated graphviz");

        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

impl ServiceInfo {
    fn graphviz_style(&self) -> &'static str {
        let is_proxy_reachable = self.is_proxy_reachable();
        match self {
            ServiceInfo::Unregistered(_) if is_proxy_reachable => "[style=solid, color=red]",
            ServiceInfo::Unregistered(_) => "[style=dashed, color=red]",
            ServiceInfo::Registered(reg) if is_proxy_reachable => "[style=solid, color=green]",
            ServiceInfo::Registered(_) => "[style=dashed, color=green]",
        }
    }
}

impl ClientInfo {
    fn graphviz_edge_label(&self, show_ends: bool) -> String {
        let client_br = self.client_br();
        let server_br = self.server_br();
        let vxlan_id = self.vxlan_id();
        let time_ms = self.time_ms();
        if show_ends {
            format!(
                "[label=\"VXLAN {vxlan_id} [{time_ms}ms]\", taillabel=\"{client_br}\", headlabel=\"{server_br}\"]"
            )
        } else {
            format!("[label=\"VXLAN {vxlan_id} [{time_ms}ms]\"]")
        }
    }
}
