#![allow(non_snake_case)]

use crate::graphviz::render_graphviz;
use crate::nullnet_grpc_impl::NullnetGrpcImpl;
use crate::services::input::{ServicesToml, apply_config_update};
use crate::services::service_info::ServiceInfo;
use crate::timeout::apply_proxy_timeouts;
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr};

fn ip(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(a, b, c, d))
}

async fn assert_net_ids_in_use(server: &NullnetGrpcImpl, expected: u32) {
    let in_use = server.orchestrator().net_ids_in_use().await;
    assert_eq!(
        in_use, expected,
        "expected {expected} NET IDs in use, got {in_use}"
    );
}

/// Strip non-deterministic parts (NET IDs, timing) from graphviz edge labels
/// so that structural comparison is possible.
fn normalize_graphviz(graphviz: &str) -> String {
    graphviz
        .lines()
        .map(|line| {
            if line.contains("->") {
                if let Some(bracket_pos) = line.find(" [label=") {
                    let mut stripped = line[..bracket_pos].to_string();
                    stripped.push(';');
                    stripped
                } else {
                    line.to_string()
                }
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn fixture_path(fixture: &str, file: &str) -> String {
    format!(
        "{}/tests/fixtures/{fixture}/{file}",
        env!("CARGO_MANIFEST_DIR")
    )
}

async fn load_fixture(fixture: &str) -> HashMap<String, ServiceInfo> {
    load_config(fixture, "services.toml").await
}

async fn load_config(fixture: &str, file: &str) -> HashMap<String, ServiceInfo> {
    ServicesToml::load_from(&fixture_path(fixture, file))
        .await
        .expect("failed to load test config")
}

async fn register_services(server: &NullnetGrpcImpl, ip_map: &HashMap<&str, IpAddr>, port: u16) {
    let mut services = server.services().write().await;
    for (&name, &svc_ip) in ip_map {
        if let Some(si) = services.get_mut(name) {
            si.add_replica(svc_ip, port, None);
        }
    }
    drop(services);

    let unique_ips: HashSet<_> = ip_map.values().collect();
    for &svc_ip in unique_ips {
        server.orchestrator().register_fake_client(svc_ip).await;
    }
}

fn assert_graphviz(services: &HashMap<String, ServiceInfo>, fixture: &str, expected_file: &str) {
    let actual = render_graphviz(services);
    let expected_path = fixture_path(fixture, expected_file);

    println!("--- {expected_file} ---\n{actual}");

    let expected = std::fs::read_to_string(&expected_path).unwrap_or_else(|_| {
        std::fs::write(&expected_path, &actual).expect("failed to write expected dot file");
        println!("BOOTSTRAPPED: wrote {expected_path}");
        actual.clone()
    });

    assert_eq!(
        normalize_graphviz(&actual),
        normalize_graphviz(&expected),
        "Graphviz mismatch for {expected_file}"
    );
}

async fn setup_proxy_chain(
    server: &NullnetGrpcImpl,
    service_name: &str,
    proxy_ip: IpAddr,
    client_ip: &str,
) {
    let (service_ip, service_docker) = {
        let guard = server.services().read().await;
        let ServiceInfo::Registered(reg) = guard.get(service_name).expect("service not found")
        else {
            panic!("service not registered");
        };
        let replica = reg.pick_replica_least_clients();
        (replica.ip(), replica.docker_container().map(String::from))
    };
    server
        .setup_proxy_chain(
            service_name,
            proxy_ip,
            client_ip,
            service_ip,
            service_docker.as_deref(),
        )
        .await
        .expect("setup_proxy_chain failed");
}

// ===========================================================================
// service_removed: A→C→D, B→D (D shared). proxy1→A+B, proxy2→A
// ===========================================================================

const SERVICE_REMOVED: &str = "service_removed";

async fn service_removed_setup() -> NullnetGrpcImpl {
    let services = load_fixture(SERVICE_REMOVED).await;
    let server = NullnetGrpcImpl::new_for_test(services);

    let ip_map = HashMap::from([
        ("A", ip(1, 1, 1, 1)),
        ("B", ip(2, 2, 2, 2)),
        ("C", ip(3, 3, 3, 3)),
        ("D", ip(4, 4, 4, 4)),
    ]);
    let proxy1 = ip(5, 5, 5, 5);
    let proxy2 = ip(6, 6, 6, 6);
    register_services(&server, &ip_map, 8080).await;
    server.orchestrator().register_fake_client(proxy1).await;
    server.orchestrator().register_fake_client(proxy2).await;

    setup_proxy_chain(&server, "A", proxy1, "10.0.0.1").await;
    setup_proxy_chain(&server, "B", proxy1, "10.0.0.1").await;
    setup_proxy_chain(&server, "A", proxy2, "10.0.0.2").await;

    // A→C, C→D, proxy1→A, B→D, proxy1→B, proxy2→A = 6 IDs
    assert_net_ids_in_use(&server, 6).await;

    let guard = server.services().read().await;
    assert_graphviz(&guard, SERVICE_REMOVED, "start.dot");
    drop(guard);

    server
}

/// Remove A from config. A and C removed (C only dep of A). D stays (dep of B).
/// proxy1→B→D survives.
#[tokio::test]
async fn service_removed_remove_A() {
    let server = service_removed_setup().await;
    let new_config = load_config(SERVICE_REMOVED, "remove_A.toml").await;

    let mut guard = server.services().write().await;
    apply_config_update(&mut guard, new_config, server.orchestrator()).await;
    assert_graphviz(&guard, SERVICE_REMOVED, "after_remove_A.dot");
    drop(guard);

    // A→C, C→D, proxy1→A, proxy2→A freed; B→D, proxy1→B survive = 2 IDs
    assert_net_ids_in_use(&server, 2).await;

    let guard = server.services().read().await;
    assert!(!guard.contains_key("A"));
    assert!(!guard.contains_key("C"));
    assert!(guard.contains_key("B"));
    assert!(guard.contains_key("D"));
}

/// Remove B from config. D stays (dep of A). A chains survive.
#[tokio::test]
async fn service_removed_remove_B() {
    let server = service_removed_setup().await;
    let new_config = load_config(SERVICE_REMOVED, "remove_B.toml").await;

    let mut guard = server.services().write().await;
    apply_config_update(&mut guard, new_config, server.orchestrator()).await;
    assert_graphviz(&guard, SERVICE_REMOVED, "after_remove_B.dot");
    drop(guard);

    // B→D, proxy1→B freed; A→C, C→D, proxy1→A, proxy2→A survive = 4 IDs
    assert_net_ids_in_use(&server, 4).await;

    let guard = server.services().read().await;
    assert!(!guard.contains_key("B"));
    assert!(guard.contains_key("A"));
    assert!(guard.contains_key("C"));
    assert!(guard.contains_key("D"));
}

// ===========================================================================
// dep_changed: A→B→C, D→C (C shared). proxy1→A+D, proxy2→A
// ===========================================================================

const DEP_CHANGED: &str = "dep_changed";

async fn dep_changed_setup() -> NullnetGrpcImpl {
    let services = load_fixture(DEP_CHANGED).await;
    let server = NullnetGrpcImpl::new_for_test(services);

    let ip_map = HashMap::from([
        ("A", ip(1, 1, 1, 1)),
        ("B", ip(2, 2, 2, 2)),
        ("C", ip(3, 3, 3, 3)),
        ("D", ip(4, 4, 4, 4)),
    ]);
    let proxy1 = ip(5, 5, 5, 5);
    let proxy2 = ip(6, 6, 6, 6);
    register_services(&server, &ip_map, 8080).await;
    server.orchestrator().register_fake_client(proxy1).await;
    server.orchestrator().register_fake_client(proxy2).await;

    setup_proxy_chain(&server, "A", proxy1, "10.0.0.1").await;
    setup_proxy_chain(&server, "D", proxy1, "10.0.0.1").await;
    setup_proxy_chain(&server, "A", proxy2, "10.0.0.2").await;

    let guard = server.services().read().await;
    assert_graphviz(&guard, DEP_CHANGED, "start.dot");
    drop(guard);

    server
}

/// Add E to A's deps: [B,C] → [B,C,E]. A's chain cleaned up (dep change).
/// E added as unregistered. D→C survives.
#[tokio::test]
async fn dep_changed_add_E_to_A() {
    let server = dep_changed_setup().await;
    let new_config = load_config(DEP_CHANGED, "add_E_to_A.toml").await;

    let mut guard = server.services().write().await;
    apply_config_update(&mut guard, new_config, server.orchestrator()).await;
    assert_graphviz(&guard, DEP_CHANGED, "after_add_E_to_A.dot");

    assert_eq!(
        guard["A"].dependencies(),
        vec!["B".to_string(), "C".to_string(), "E".to_string()]
    );
    assert!(guard.contains_key("E"));
    assert!(matches!(guard["E"], ServiceInfo::Unregistered(_)));
}

/// Drop C from A's deps: [B,C] → [B]. A's chain cleaned up but A stays registered.
/// D→C survives.
#[tokio::test]
async fn dep_changed_drop_C_from_A() {
    let server = dep_changed_setup().await;
    let new_config = load_config(DEP_CHANGED, "drop_C_from_A.toml").await;

    let mut guard = server.services().write().await;
    apply_config_update(&mut guard, new_config, server.orchestrator()).await;
    assert_graphviz(&guard, DEP_CHANGED, "after_drop_C_from_A.dot");

    assert!(guard.contains_key("A"));
    assert_eq!(guard["A"].dependencies(), vec!["B".to_string()]);
    assert!(guard.contains_key("C"));
}

/// Drop all deps from D: [C] → []. D's chain cleaned up but D stays registered.
/// A chains survive.
#[tokio::test]
async fn dep_changed_drop_all_from_D() {
    let server = dep_changed_setup().await;
    let new_config = load_config(DEP_CHANGED, "drop_all_from_D.toml").await;

    let mut guard = server.services().write().await;
    apply_config_update(&mut guard, new_config, server.orchestrator()).await;
    assert_graphviz(&guard, DEP_CHANGED, "after_drop_all_from_D.dot");

    assert!(guard["D"].dependencies().is_empty());
    assert!(guard.contains_key("C"));
}

/// Swap C for E in A's deps: [B,C] → [B,E]. A's chain cleaned up.
/// E added as unregistered. D→C survives.
#[tokio::test]
async fn dep_changed_swap_C_for_E() {
    let server = dep_changed_setup().await;
    let new_config = load_config(DEP_CHANGED, "swap_C_for_E.toml").await;

    let mut guard = server.services().write().await;
    apply_config_update(&mut guard, new_config, server.orchestrator()).await;
    assert_graphviz(&guard, DEP_CHANGED, "after_swap_C_for_E.dot");

    assert_eq!(
        guard["A"].dependencies(),
        vec!["B".to_string(), "E".to_string()]
    );
    assert!(guard.contains_key("E"));
    assert!(matches!(guard["E"], ServiceInfo::Unregistered(_)));
}

// ===========================================================================
// reachability_changed: A→B→C, D→E. proxy1→A+D, proxy2→B.
// B is both proxy-reachable and a dep of A.
// ===========================================================================

const REACHABILITY_CHANGED: &str = "reachability_changed";

async fn reachability_changed_setup() -> NullnetGrpcImpl {
    let services = load_fixture(REACHABILITY_CHANGED).await;
    let server = NullnetGrpcImpl::new_for_test(services);

    let ip_map = HashMap::from([
        ("A", ip(1, 1, 1, 1)),
        ("B", ip(2, 2, 2, 2)),
        ("C", ip(3, 3, 3, 3)),
        ("D", ip(4, 4, 4, 4)),
        ("E", ip(5, 5, 5, 5)),
    ]);
    let proxy1 = ip(6, 6, 6, 6);
    let proxy2 = ip(7, 7, 7, 7);
    register_services(&server, &ip_map, 8080).await;
    server.orchestrator().register_fake_client(proxy1).await;
    server.orchestrator().register_fake_client(proxy2).await;

    setup_proxy_chain(&server, "A", proxy1, "10.0.0.1").await;
    setup_proxy_chain(&server, "D", proxy1, "10.0.0.1").await;
    setup_proxy_chain(&server, "B", proxy2, "10.0.0.2").await;

    let guard = server.services().read().await;
    assert_graphviz(&guard, REACHABILITY_CHANGED, "start.dot");
    drop(guard);

    server
}

/// B becomes unreachable (loses its [[services]] entry). B's own proxy chain
/// (proxy2→B) is torn down, but A's chain survives because B's deps are
/// correctly inferred from A's dependency list. D→E also survives.
#[tokio::test]
async fn reachability_changed_unreachable_B() {
    let server = reachability_changed_setup().await;
    let new_config = load_config(REACHABILITY_CHANGED, "unreachable_B.toml").await;

    let mut guard = server.services().write().await;
    apply_config_update(&mut guard, new_config, server.orchestrator()).await;
    assert_graphviz(&guard, REACHABILITY_CHANGED, "after_unreachable_B.dot");

    assert!(guard.contains_key("B"));
    assert!(guard["B"].is_proxy_reachable().is_none());
}

/// D removed from [[services]] and no other service depends on it, so D and E
/// are fully removed from the map. proxy1→D and D→E torn down.
/// A and B chains survive.
#[tokio::test]
async fn reachability_changed_unreachable_D() {
    let server = reachability_changed_setup().await;
    let new_config = load_config(REACHABILITY_CHANGED, "unreachable_D.toml").await;

    let mut guard = server.services().write().await;
    apply_config_update(&mut guard, new_config, server.orchestrator()).await;
    assert_graphviz(&guard, REACHABILITY_CHANGED, "after_unreachable_D.dot");

    assert!(!guard.contains_key("D"));
    assert!(!guard.contains_key("E"));
}

// ===========================================================================
// service_unregistered: A→C→D, B→D (D shared). A+B co-located at 1.1.1.1.
// proxy1→A+B, proxy2→A
// ===========================================================================

const SERVICE_UNREGISTERED: &str = "service_unregistered";

async fn service_unregistered_setup() -> NullnetGrpcImpl {
    let services = load_fixture(SERVICE_UNREGISTERED).await;
    let server = NullnetGrpcImpl::new_for_test(services);

    // A and B co-located at 1.1.1.1
    let ip_map = HashMap::from([
        ("A", ip(1, 1, 1, 1)),
        ("B", ip(1, 1, 1, 1)),
        ("C", ip(2, 2, 2, 2)),
        ("D", ip(3, 3, 3, 3)),
    ]);
    let proxy1 = ip(5, 5, 5, 5);
    let proxy2 = ip(6, 6, 6, 6);
    register_services(&server, &ip_map, 8080).await;
    server.orchestrator().register_fake_client(proxy1).await;
    server.orchestrator().register_fake_client(proxy2).await;

    setup_proxy_chain(&server, "A", proxy1, "10.0.0.1").await;
    setup_proxy_chain(&server, "B", proxy1, "10.0.0.1").await;
    setup_proxy_chain(&server, "A", proxy2, "10.0.0.2").await;

    assert_net_ids_in_use(&server, 6).await;

    let guard = server.services().read().await;
    assert_graphviz(&guard, SERVICE_UNREGISTERED, "start.dot");
    drop(guard);

    server
}

/// Node 1.1.1.1 re-registers with only B (drops A selectively).
/// A's chains cleaned up. B→D survives.
#[tokio::test]
async fn service_unregistered_drop_A() {
    let server = service_unregistered_setup().await;

    server
        .apply_services_list(ip(1, 1, 1, 1), &[("B".into(), 8080, None)])
        .await
        .expect("apply_services_list failed");

    let guard = server.services().read().await;
    assert_graphviz(&guard, SERVICE_UNREGISTERED, "after_drop_A.dot");

    assert!(matches!(guard["A"], ServiceInfo::Unregistered(_)));
    assert!(matches!(guard["B"], ServiceInfo::Registered(_)));
}

/// Node 1.1.1.1 re-registers with only A (drops B selectively).
/// B's chain cleaned up. A chains survive.
#[tokio::test]
async fn service_unregistered_drop_B() {
    let server = service_unregistered_setup().await;

    server
        .apply_services_list(ip(1, 1, 1, 1), &[("A".into(), 8080, None)])
        .await
        .expect("apply_services_list failed");

    let guard = server.services().read().await;
    assert_graphviz(&guard, SERVICE_UNREGISTERED, "after_drop_B.dot");

    assert!(matches!(guard["A"], ServiceInfo::Registered(_)));
    assert!(matches!(guard["B"], ServiceInfo::Unregistered(_)));
}

/// Leaf dep host 2.2.2.2 re-registers with empty list (C unregistered).
/// Cascades to A (depends on C). B→D survives (B doesn't depend on C).
#[tokio::test]
async fn service_unregistered_drop_C() {
    let server = service_unregistered_setup().await;

    server
        .apply_services_list(ip(2, 2, 2, 2), &[])
        .await
        .expect("apply_services_list failed");

    let guard = server.services().read().await;
    assert_graphviz(&guard, SERVICE_UNREGISTERED, "after_drop_C.dot");

    assert!(matches!(guard["C"], ServiceInfo::Unregistered(_)));
    assert!(matches!(guard["B"], ServiceInfo::Registered(_)));
}

/// Shared dep host 3.3.3.3 re-registers with empty list (D unregistered).
/// Cascades to A (deps on D) and B (deps on D). All chains torn down.
/// All 6 NET IDs freed.
#[tokio::test]
async fn service_unregistered_drop_D() {
    let server = service_unregistered_setup().await;

    server
        .apply_services_list(ip(3, 3, 3, 3), &[])
        .await
        .expect("apply_services_list failed");

    let guard = server.services().read().await;
    assert_graphviz(&guard, SERVICE_UNREGISTERED, "after_drop_D.dot");
    drop(guard);

    // all 6 IDs freed
    assert_net_ids_in_use(&server, 0).await;

    let guard = server.services().read().await;
    assert!(matches!(guard["D"], ServiceInfo::Unregistered(_)));
}

// ===========================================================================
// node_disconnected: same topology as service_unregistered (A+B co-located).
// Contrasts: disconnect kills ALL services at that IP.
// ===========================================================================

const NODE_DISCONNECTED: &str = "node_disconnected";

async fn node_disconnected_setup() -> NullnetGrpcImpl {
    let services = load_fixture(NODE_DISCONNECTED).await;
    let server = NullnetGrpcImpl::new_for_test(services);

    // A and B co-located at 1.1.1.1
    let ip_map = HashMap::from([
        ("A", ip(1, 1, 1, 1)),
        ("B", ip(1, 1, 1, 1)),
        ("C", ip(2, 2, 2, 2)),
        ("D", ip(3, 3, 3, 3)),
    ]);
    let proxy1 = ip(5, 5, 5, 5);
    let proxy2 = ip(6, 6, 6, 6);
    register_services(&server, &ip_map, 8080).await;
    server.orchestrator().register_fake_client(proxy1).await;
    server.orchestrator().register_fake_client(proxy2).await;

    setup_proxy_chain(&server, "A", proxy1, "10.0.0.1").await;
    setup_proxy_chain(&server, "B", proxy1, "10.0.0.1").await;
    setup_proxy_chain(&server, "A", proxy2, "10.0.0.2").await;

    assert_net_ids_in_use(&server, 6).await;

    let guard = server.services().read().await;
    assert_graphviz(&guard, NODE_DISCONNECTED, "start.dot");
    drop(guard);

    server
}

/// A+B host (1.1.1.1) disconnects. BOTH A and B cleaned up (unlike
/// service_unregistered which can drop selectively).
/// All 6 NET IDs freed.
#[tokio::test]
async fn node_disconnected_A_B() {
    let server = node_disconnected_setup().await;

    server
        .orchestrator()
        .handle_node_disconnect(ip(1, 1, 1, 1), server.services())
        .await;

    let guard = server.services().read().await;
    assert_graphviz(&guard, NODE_DISCONNECTED, "after_disconnect_A_B.dot");
    drop(guard);

    assert_net_ids_in_use(&server, 0).await;

    let guard = server.services().read().await;
    assert!(matches!(guard["A"], ServiceInfo::Unregistered(_)));
    assert!(matches!(guard["B"], ServiceInfo::Unregistered(_)));
}

/// C host (2.2.2.2) disconnects. C cleaned up, cascades to A (depends on C).
/// B→D survives (B doesn't depend on C).
#[tokio::test]
async fn node_disconnected_C() {
    let server = node_disconnected_setup().await;

    server
        .orchestrator()
        .handle_node_disconnect(ip(2, 2, 2, 2), server.services())
        .await;

    let guard = server.services().read().await;
    assert_graphviz(&guard, NODE_DISCONNECTED, "after_disconnect_C.dot");

    assert!(matches!(guard["C"], ServiceInfo::Unregistered(_)));
    assert!(matches!(guard["B"], ServiceInfo::Registered(_)));
}

/// D host (3.3.3.3) disconnects. D cleaned up, cascades to A and B (both depend on D).
#[tokio::test]
async fn node_disconnected_D() {
    let server = node_disconnected_setup().await;

    server
        .orchestrator()
        .handle_node_disconnect(ip(3, 3, 3, 3), server.services())
        .await;

    let guard = server.services().read().await;
    assert_graphviz(&guard, NODE_DISCONNECTED, "after_disconnect_D.dot");

    assert!(matches!(guard["D"], ServiceInfo::Unregistered(_)));
}

/// Proxy1 (5.5.5.5) disconnects. proxy1→A and proxy1→B chains torn down.
/// proxy2→A survives, so A→C→D edges survive. All services stay registered.
#[tokio::test]
async fn node_disconnected_proxy1() {
    let server = node_disconnected_setup().await;

    server
        .orchestrator()
        .handle_node_disconnect(ip(5, 5, 5, 5), server.services())
        .await;

    let guard = server.services().read().await;
    assert_graphviz(&guard, NODE_DISCONNECTED, "after_disconnect_proxy1.dot");
    drop(guard);

    // proxy1→A, proxy1→B, B→D freed; A→C, C→D, proxy2→A survive = 3 IDs
    assert_net_ids_in_use(&server, 3).await;

    let guard = server.services().read().await;
    assert!(matches!(guard["A"], ServiceInfo::Registered(_)));
    assert!(matches!(guard["B"], ServiceInfo::Registered(_)));
    assert!(matches!(guard["C"], ServiceInfo::Registered(_)));
    assert!(matches!(guard["D"], ServiceInfo::Registered(_)));
}

// ===========================================================================
// proxy_timeout: A→C→D, B→D (D shared). proxy1→A+B, proxy2→A.
// A has timeout=1, B has timeout=2.
// ===========================================================================

const PROXY_TIMEOUT: &str = "proxy_timeout";

async fn proxy_timeout_setup() -> NullnetGrpcImpl {
    let services = load_fixture(PROXY_TIMEOUT).await;
    let server = NullnetGrpcImpl::new_for_test(services);

    let ip_map = HashMap::from([
        ("A", ip(1, 1, 1, 1)),
        ("B", ip(2, 2, 2, 2)),
        ("C", ip(3, 3, 3, 3)),
        ("D", ip(4, 4, 4, 4)),
    ]);
    let proxy1 = ip(5, 5, 5, 5);
    let proxy2 = ip(6, 6, 6, 6);
    register_services(&server, &ip_map, 8080).await;
    server.orchestrator().register_fake_client(proxy1).await;
    server.orchestrator().register_fake_client(proxy2).await;

    setup_proxy_chain(&server, "A", proxy1, "10.0.0.1").await;
    setup_proxy_chain(&server, "B", proxy1, "10.0.0.1").await;
    setup_proxy_chain(&server, "A", proxy2, "10.0.0.2").await;

    assert_net_ids_in_use(&server, 6).await;

    let guard = server.services().read().await;
    assert_graphviz(&guard, PROXY_TIMEOUT, "start.dot");
    drop(guard);

    server
}

/// After A's timeout (1s), both proxy clients on A expire. B's proxy client
/// survives (timeout=2). A→C→D edges removed (no more proxy clients on A).
/// B→D edge survives.
#[tokio::test]
async fn proxy_timeout_A() {
    let server = proxy_timeout_setup().await;

    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

    let mut guard = server.services().write().await;
    apply_proxy_timeouts(&mut guard, server.orchestrator()).await;
    assert_graphviz(&guard, PROXY_TIMEOUT, "after_timeout_A.dot");

    // A is still registered but has no proxy clients
    assert!(matches!(guard["A"], ServiceInfo::Registered(_)));
    // B's proxy client is still alive
    assert!(matches!(guard["B"], ServiceInfo::Registered(_)));
    if let ServiceInfo::Registered(reg) = &guard["B"] {
        assert_eq!(reg.client_count(), 1);
    }
    if let ServiceInfo::Registered(reg) = &guard["A"] {
        assert!(!reg.has_clients());
    }
}

/// After B's timeout (2s), B's proxy client also expires.
/// All proxy chains gone; all services still registered.
#[tokio::test]
async fn proxy_timeout_A_then_B() {
    let server = proxy_timeout_setup().await;

    // A expires after 1s
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let mut guard = server.services().write().await;
    apply_proxy_timeouts(&mut guard, server.orchestrator()).await;
    assert_graphviz(&guard, PROXY_TIMEOUT, "after_timeout_A.dot");
    drop(guard);

    // B expires after 2s total
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let mut guard = server.services().write().await;
    apply_proxy_timeouts(&mut guard, server.orchestrator()).await;
    assert_graphviz(&guard, PROXY_TIMEOUT, "after_timeout_A_then_B.dot");

    // all services still registered, but no proxy clients left
    for (_, si) in guard.iter() {
        if let ServiceInfo::Registered(reg) = si {
            assert!(
                !reg.has_clients(),
                "expected no proxy clients after both timeouts"
            );
        }
    }
    drop(guard);

    // all 6 IDs freed
    assert_net_ids_in_use(&server, 0).await;
}

/// After 2s+ both A and B expire simultaneously in a single apply.
/// All 6 NET IDs freed.
#[tokio::test]
async fn proxy_timeout_all_at_once() {
    let server = proxy_timeout_setup().await;

    tokio::time::sleep(std::time::Duration::from_millis(2100)).await;

    let mut guard = server.services().write().await;
    apply_proxy_timeouts(&mut guard, server.orchestrator()).await;
    assert_graphviz(&guard, PROXY_TIMEOUT, "after_timeout_all.dot");

    for (_, si) in guard.iter() {
        if let ServiceInfo::Registered(reg) = si {
            assert!(
                !reg.has_clients(),
                "expected no proxy clients after full timeout"
            );
        }
    }
    drop(guard);

    assert_net_ids_in_use(&server, 0).await;
}

/// Config update tightens B's timeout from 2→1. After 1.5s, B's clients
/// are past the new limit and get expired by the config change.
/// A's timeout is unchanged in the config, so the config path doesn't
/// touch A's clients (even though they're past A's own timeout).
#[tokio::test]
async fn proxy_timeout_config_tighten_B() {
    let server = proxy_timeout_setup().await;

    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

    let new_config = load_config(PROXY_TIMEOUT, "tighten_B.toml").await;
    let mut guard = server.services().write().await;
    apply_config_update(&mut guard, new_config, server.orchestrator()).await;
    assert_graphviz(&guard, PROXY_TIMEOUT, "after_config_tighten_B.dot");

    // B's proxy client expired due to config tightening
    if let ServiceInfo::Registered(reg) = &guard["B"] {
        assert!(!reg.has_clients());
    }
    // A's clients are still present (config path only handles config changes)
    if let ServiceInfo::Registered(reg) = &guard["A"] {
        assert_eq!(reg.client_count(), 2);
    }
}

/// Config update removes A's timeout (1→0). Even after 1.5s, A's clients
/// are NOT expired because the timeout was removed, not tightened.
/// B's timeout is unchanged, so B's client also stays.
#[tokio::test]
async fn proxy_timeout_config_remove_timeout_A() {
    let server = proxy_timeout_setup().await;

    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

    let new_config = load_config(PROXY_TIMEOUT, "remove_timeout_A.toml").await;
    let mut guard = server.services().write().await;
    apply_config_update(&mut guard, new_config, server.orchestrator()).await;
    assert_graphviz(&guard, PROXY_TIMEOUT, "after_config_remove_timeout_A.dot");

    // A's timeout was removed — no expiry, clients still present
    assert_eq!(guard["A"].is_proxy_reachable(), Some(0));
    if let ServiceInfo::Registered(reg) = &guard["A"] {
        assert_eq!(reg.client_count(), 2);
    }
    // B unchanged
    if let ServiceInfo::Registered(reg) = &guard["B"] {
        assert_eq!(reg.client_count(), 1);
    }
}

// ===========================================================================
// multi_replica: A→B, C→B. B has 3 replicas across 2 IPs:
//   - 2.2.2.2 container "b1"
//   - 2.2.2.2 container "b2"  (Docker Swarm: two containers, same host)
//   - 4.4.4.4 (no container)
// Tests round-robin, sticky sessions, and partial replica removal.
// ===========================================================================

const MULTI_REPLICA: &str = "multi_replica";

async fn multi_replica_setup() -> NullnetGrpcImpl {
    let services = load_fixture(MULTI_REPLICA).await;
    let server = NullnetGrpcImpl::new_for_test(services);

    // A at 1.1.1.1, C at 3.3.3.3
    let ip_map = HashMap::from([("A", ip(1, 1, 1, 1)), ("C", ip(3, 3, 3, 3))]);
    register_services(&server, &ip_map, 8080).await;

    // B has 3 replicas across 2 IPs:
    //   2.2.2.2 "b1", 4.4.4.4 (standalone), 2.2.2.2 "b2" (same host, Docker Swarm)
    // Insertion order matters for least-clients tie-breaking (first with min wins),
    // so 4.4.4.4 is inserted between the two Docker Swarm containers to ensure
    // least-clients distributes across IPs.
    {
        let mut services = server.services().write().await;
        services
            .get_mut("B")
            .unwrap()
            .add_replica(ip(2, 2, 2, 2), 8080, Some("b1".into()));
        services
            .get_mut("B")
            .unwrap()
            .add_replica(ip(4, 4, 4, 4), 8080, None);
        services
            .get_mut("B")
            .unwrap()
            .add_replica(ip(2, 2, 2, 2), 8080, Some("b2".into()));
    }
    server
        .orchestrator()
        .register_fake_client(ip(2, 2, 2, 2))
        .await;
    server
        .orchestrator()
        .register_fake_client(ip(4, 4, 4, 4))
        .await;

    server
}

/// B has 3 replicas across 2 IPs. Verify all are present.
#[tokio::test]
async fn multi_replica_register() {
    let server = multi_replica_setup().await;
    let guard = server.services().read().await;

    let ServiceInfo::Registered(reg) = &guard["B"] else {
        panic!("B should be registered");
    };
    assert_eq!(reg.replicas().len(), 3);
    // 2 replicas on 2.2.2.2 (containers "b1" and "b2"), 1 on 4.4.4.4
    assert!(reg.has_replica_on_ip(ip(2, 2, 2, 2)));
    assert!(reg.has_replica_on_ip(ip(4, 4, 4, 4)));
    let on_2: Vec<_> = reg
        .replicas()
        .iter()
        .filter(|r| r.ip() == ip(2, 2, 2, 2))
        .collect();
    assert_eq!(
        on_2.len(),
        2,
        "2.2.2.2 should have 2 replicas (Docker Swarm)"
    );
    let containers: HashSet<_> = on_2.iter().filter_map(|r| r.docker_container()).collect();
    assert!(containers.contains("b1"));
    assert!(containers.contains("b2"));
}

/// Least-clients: proxy requests and dependency chains pick the replica
/// with the fewest active clients.  After two proxy chains (A and C both
/// depend on B), B's 3 replicas should spread: the second chain picks a
/// different replica than the first.
#[tokio::test]
async fn multi_replica_least_clients() {
    let server = multi_replica_setup().await;
    let proxy1 = ip(5, 5, 5, 5);
    server.orchestrator().register_fake_client(proxy1).await;

    // First chain: proxy1 -> A -> B (picks least-loaded B replica, all empty)
    setup_proxy_chain(&server, "A", proxy1, "10.0.0.1").await;

    {
        let guard = server.services().read().await;
        let ServiceInfo::Registered(reg_b) = &guard["B"] else {
            panic!("B should be registered");
        };
        assert_eq!(
            reg_b.client_count(),
            1,
            "B should have 1 client after first chain"
        );
    }

    // Second chain: proxy1 -> C -> B (picks a *different* B replica since the
    // first now has 1 client while two others have 0)
    setup_proxy_chain(&server, "C", proxy1, "10.0.0.2").await;

    let guard = server.services().read().await;
    assert_graphviz(&guard, MULTI_REPLICA, "start.dot");

    let ServiceInfo::Registered(reg_b) = &guard["B"] else {
        panic!("B should be registered");
    };

    // B should now have 2 clients spread across replicas
    assert_eq!(reg_b.client_count(), 2, "B should have 2 clients total");

    // No single replica should hold both
    for replica in reg_b.replicas() {
        assert!(
            replica.clients().len() <= 1,
            "each B replica should have at most 1 client, got {}",
            replica.clients().len()
        );
    }
}

/// Sticky session: same proxy client reconnects to the same replica.
#[tokio::test]
async fn multi_replica_sticky_session() {
    let server = multi_replica_setup().await;
    let proxy1 = ip(5, 5, 5, 5);
    server.orchestrator().register_fake_client(proxy1).await;

    // First request from client 10.0.0.1
    setup_proxy_chain(&server, "A", proxy1, "10.0.0.1").await;

    // Record which upstream the client got
    let first_upstream = {
        let guard = server.services().read().await;
        let ServiceInfo::Registered(reg) = &guard["A"] else {
            panic!("A should be registered");
        };
        let client = crate::services::clients::Client::new("10.0.0.1".to_string(), Some(proxy1));
        reg.is_client_setup(&client)
            .expect("client should be set up")
    };

    // Second lookup from same client — should be sticky (same upstream)
    let second_upstream = {
        let guard = server.services().read().await;
        let ServiceInfo::Registered(reg) = &guard["A"] else {
            panic!("A should be registered");
        };
        let client = crate::services::clients::Client::new("10.0.0.1".to_string(), Some(proxy1));
        reg.is_client_setup(&client)
            .expect("client should still be set up")
    };

    assert_eq!(
        first_upstream, second_upstream,
        "sticky session should return same upstream"
    );
}

/// Partial replica removal: disconnect 2.2.2.2 removes two of B's replicas
/// ("b1" and "b2"), but B stays registered via the replica at 4.4.4.4.
///
/// With least-clients distribution:
///   A→B lands on "b1" (2.2.2.2)  — affected by disconnect
///   C→B lands on 4.4.4.4         — NOT affected, chain survives
#[tokio::test]
async fn multi_replica_partial_disconnect() {
    let server = multi_replica_setup().await;
    let proxy1 = ip(5, 5, 5, 5);
    server.orchestrator().register_fake_client(proxy1).await;

    setup_proxy_chain(&server, "A", proxy1, "10.0.0.1").await;
    setup_proxy_chain(&server, "C", proxy1, "10.0.0.2").await;

    // A→B, proxy→A, C→B, proxy→C = 4 NET IDs
    assert_net_ids_in_use(&server, 4).await;

    {
        let guard = server.services().read().await;
        assert_graphviz(&guard, MULTI_REPLICA, "start.dot");
    }

    // Disconnect 2.2.2.2 — removes "b1" and "b2" replicas.
    // Only A→B (on "b1") is affected; C→B (on 4.4.4.4) survives.
    server
        .orchestrator()
        .handle_node_disconnect(ip(2, 2, 2, 2), server.services())
        .await;

    let guard = server.services().read().await;
    assert_graphviz(&guard, MULTI_REPLICA, "after_partial_disconnect.dot");

    // B should still be registered (has replica at 4.4.4.4)
    assert!(
        matches!(guard["B"], ServiceInfo::Registered(_)),
        "B should still be registered with remaining replica"
    );
    if let ServiceInfo::Registered(reg) = &guard["B"] {
        assert_eq!(reg.replicas().len(), 1, "B should have 1 replica left");
        assert!(reg.has_replica_on_ip(ip(4, 4, 4, 4)));
        assert!(!reg.has_replica_on_ip(ip(2, 2, 2, 2)));
        // C→B on 4.4.4.4 survived — B still has 1 client
        assert_eq!(reg.client_count(), 1, "C→B chain should survive");
    }

    // A's chain was torn down (A→B was on removed replica)
    if let ServiceInfo::Registered(reg) = &guard["A"] {
        assert!(!reg.has_clients(), "A should have no proxy clients");
    }

    // C's chain survived (C→B was on 4.4.4.4)
    if let ServiceInfo::Registered(reg) = &guard["C"] {
        assert_eq!(
            reg.client_count(),
            1,
            "C should still have its proxy client"
        );
    }

    // A→B and proxy→A freed; C→B and proxy→C survive = 2 NET IDs
    drop(guard);
    assert_net_ids_in_use(&server, 2).await;
}

/// Full replica removal: disconnect 2.2.2.2 (removes "b1" + "b2"), then
/// disconnect 4.4.4.4 (removes last replica). B becomes unregistered and
/// cascades to A and C.
#[tokio::test]
async fn multi_replica_full_disconnect() {
    let server = multi_replica_setup().await;
    let proxy1 = ip(5, 5, 5, 5);
    server.orchestrator().register_fake_client(proxy1).await;

    setup_proxy_chain(&server, "A", proxy1, "10.0.0.1").await;
    setup_proxy_chain(&server, "C", proxy1, "10.0.0.2").await;

    {
        let guard = server.services().read().await;
        assert_graphviz(&guard, MULTI_REPLICA, "start.dot");
    }

    // Disconnect 2.2.2.2 (partial) — removes 2 of 3 replicas, cascades chains through B
    server
        .orchestrator()
        .handle_node_disconnect(ip(2, 2, 2, 2), server.services())
        .await;

    {
        let guard = server.services().read().await;
        assert_graphviz(&guard, MULTI_REPLICA, "after_partial_disconnect.dot");
        // B should still be registered (has replica at 4.4.4.4)
        assert!(matches!(guard["B"], ServiceInfo::Registered(_)));
        if let ServiceInfo::Registered(reg) = &guard["B"] {
            assert_eq!(reg.replicas().len(), 1);
        }
    }

    // Disconnect 4.4.4.4 — last replica, full teardown
    server
        .orchestrator()
        .handle_node_disconnect(ip(4, 4, 4, 4), server.services())
        .await;

    let guard = server.services().read().await;
    assert_graphviz(&guard, MULTI_REPLICA, "after_full_disconnect.dot");

    // B should be unregistered (no replicas)
    assert!(
        matches!(guard["B"], ServiceInfo::Unregistered(_)),
        "B should be unregistered with no replicas"
    );

    // All NET IDs should be freed
    drop(guard);
    assert_net_ids_in_use(&server, 0).await;
}

/// ServicesList from two hosts: host 2.2.2.2 sends two containers ("b1", "b2"),
/// host 4.4.4.4 sends one standalone replica. Then host 2.2.2.2 re-registers
/// with empty list — both its replicas are removed, but 4.4.4.4's stays.
#[tokio::test]
async fn multi_replica_via_services_list() {
    let services = load_fixture(MULTI_REPLICA).await;
    let server = NullnetGrpcImpl::new_for_test(services);

    // Host 2.2.2.2 registers B in two containers
    server
        .orchestrator()
        .register_fake_client(ip(2, 2, 2, 2))
        .await;
    server
        .apply_services_list(
            ip(2, 2, 2, 2),
            &[
                ("B".into(), 8080, Some("b1".into())),
                ("B".into(), 8080, Some("b2".into())),
            ],
        )
        .await
        .expect("apply_services_list failed");

    // Host 4.4.4.4 registers B standalone
    server
        .orchestrator()
        .register_fake_client(ip(4, 4, 4, 4))
        .await;
    server
        .apply_services_list(ip(4, 4, 4, 4), &[("B".into(), 9090, None)])
        .await
        .expect("apply_services_list failed");

    {
        let guard = server.services().read().await;
        let ServiceInfo::Registered(reg) = &guard["B"] else {
            panic!("B should be registered");
        };
        assert_eq!(reg.replicas().len(), 3);
        let on_2: Vec<_> = reg
            .replicas()
            .iter()
            .filter(|r| r.ip() == ip(2, 2, 2, 2))
            .collect();
        assert_eq!(on_2.len(), 2, "2.2.2.2 should have 2 replicas");
        assert!(reg.has_replica_on_ip(ip(4, 4, 4, 4)));
    }

    // Host 2.2.2.2 re-registers WITHOUT B → both its replicas removed
    server
        .apply_services_list(ip(2, 2, 2, 2), &[])
        .await
        .expect("apply_services_list failed");

    let guard = server.services().read().await;
    let ServiceInfo::Registered(reg) = &guard["B"] else {
        panic!("B should still be registered");
    };
    assert_eq!(
        reg.replicas().len(),
        1,
        "only 4.4.4.4 replica should remain"
    );
    assert!(reg.has_replica_on_ip(ip(4, 4, 4, 4)));
    assert!(!reg.has_replica_on_ip(ip(2, 2, 2, 2)));
}

/// Docker Swarm: host 2.2.2.2 runs containers "b1" and "b2". Container "b1"
/// dies, so the host re-registers with only "b2". Only "b1" is removed;
/// "b2" and the standalone 4.4.4.4 replica survive.
#[tokio::test]
async fn multi_replica_single_container_removed() {
    let services = load_fixture(MULTI_REPLICA).await;
    let server = NullnetGrpcImpl::new_for_test(services);

    // Host 2.2.2.2 registers B in two containers
    server
        .orchestrator()
        .register_fake_client(ip(2, 2, 2, 2))
        .await;
    server
        .apply_services_list(
            ip(2, 2, 2, 2),
            &[
                ("B".into(), 8080, Some("b1".into())),
                ("B".into(), 8080, Some("b2".into())),
            ],
        )
        .await
        .expect("apply_services_list failed");

    // Host 4.4.4.4 registers B standalone
    server
        .orchestrator()
        .register_fake_client(ip(4, 4, 4, 4))
        .await;
    server
        .apply_services_list(ip(4, 4, 4, 4), &[("B".into(), 9090, None)])
        .await
        .expect("apply_services_list failed");

    {
        let guard = server.services().read().await;
        let ServiceInfo::Registered(reg) = &guard["B"] else {
            panic!("B should be registered");
        };
        assert_eq!(reg.replicas().len(), 3);
    }

    // Container "b1" dies — host re-registers with only "b2"
    server
        .apply_services_list(ip(2, 2, 2, 2), &[("B".into(), 8080, Some("b2".into()))])
        .await
        .expect("apply_services_list failed");

    let guard = server.services().read().await;
    let ServiceInfo::Registered(reg) = &guard["B"] else {
        panic!("B should still be registered");
    };

    // "b1" removed, "b2" and 4.4.4.4 survive → 2 replicas left
    assert_eq!(reg.replicas().len(), 2, "should have 2 replicas left");
    assert!(
        reg.has_replica_on_ip(ip(2, 2, 2, 2)),
        "2.2.2.2 should still have a replica"
    );
    assert!(
        reg.has_replica_on_ip(ip(4, 4, 4, 4)),
        "4.4.4.4 should still have a replica"
    );

    // The surviving 2.2.2.2 replica should be "b2"
    let on_2: Vec<_> = reg
        .replicas()
        .iter()
        .filter(|r| r.ip() == ip(2, 2, 2, 2))
        .collect();
    assert_eq!(on_2.len(), 1, "only one replica on 2.2.2.2");
    assert_eq!(on_2[0].docker_container(), Some("b2"));
}
