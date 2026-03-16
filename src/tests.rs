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
            si.register(svc_ip, port);
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
    server
        .setup_proxy_chain(service_name, proxy_ip, client_ip)
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
        .apply_services_list(ip(1, 1, 1, 1), &[("B".into(), 8080)])
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
        .apply_services_list(ip(1, 1, 1, 1), &[("A".into(), 8080)])
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
#[tokio::test]
async fn service_unregistered_drop_D() {
    let server = service_unregistered_setup().await;

    server
        .apply_services_list(ip(3, 3, 3, 3), &[])
        .await
        .expect("apply_services_list failed");

    let guard = server.services().read().await;
    assert_graphviz(&guard, SERVICE_UNREGISTERED, "after_drop_D.dot");

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

    let guard = server.services().read().await;
    assert_graphviz(&guard, NODE_DISCONNECTED, "start.dot");
    drop(guard);

    server
}

/// A+B host (1.1.1.1) disconnects. BOTH A and B cleaned up (unlike
/// service_unregistered which can drop selectively).
#[tokio::test]
async fn node_disconnected_A_B() {
    let server = node_disconnected_setup().await;

    server
        .orchestrator()
        .handle_node_disconnect(ip(1, 1, 1, 1), server.services())
        .await;

    let guard = server.services().read().await;
    assert_graphviz(&guard, NODE_DISCONNECTED, "after_disconnect_A_B.dot");

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
        assert_eq!(reg.clients().len(), 1);
    }
    if let ServiceInfo::Registered(reg) = &guard["A"] {
        assert!(reg.clients().is_empty());
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
                reg.clients().is_empty(),
                "expected no proxy clients after both timeouts"
            );
        }
    }
}

/// After 2s+ both A and B expire simultaneously in a single apply.
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
                reg.clients().is_empty(),
                "expected no proxy clients after full timeout"
            );
        }
    }
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
        assert!(reg.clients().is_empty());
    }
    // A's clients are still present (config path only handles config changes)
    if let ServiceInfo::Registered(reg) = &guard["A"] {
        assert_eq!(reg.clients().len(), 2);
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
        assert_eq!(reg.clients().len(), 2);
    }
    // B unchanged
    if let ServiceInfo::Registered(reg) = &guard["B"] {
        assert_eq!(reg.clients().len(), 1);
    }
}
