//! Opt-in DNS-SD/mDNS discovery for nearby atmux web nodes.
//!
//! The multicast advertisement deliberately contains only a stable machine id,
//! human label, and directly resolved LAN address. It carries no token and
//! never accepts a URL from an advertisement. Every control request still uses
//! the normal authenticated HTTP transport.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    hash::{DefaultHasher, Hash, Hasher},
    net::{Ipv4Addr, SocketAddr},
    time::{Duration, Instant},
};

use crate::{
    config::Config,
    control::ControlPlane,
    machine::{Secret, validate_machine_id, validate_machine_label},
    remote::RemoteMachine,
};
use anyhow::{Context, Result, bail};
use mdns_sd::{ResolvedService, ServiceDaemon, ServiceEvent, ServiceInfo};
use tokio::task::JoinHandle;

/// Private DNS-SD service type used only by atmux peers.
pub const SERVICE_TYPE: &str = "_atmux._tcp.local.";
const MAX_DISCOVERED_SERVICES: usize = 64;
const MAX_DISCOVERED_MACHINES: usize = 16;
const DISCOVERED_SERVICE_MAX_AGE: Duration = Duration::from_secs(10 * 60);

#[derive(Clone, Debug, PartialEq, Eq)]
struct DiscoveredNode {
    fullname: String,
    id: String,
    label: String,
    address: Ipv4Addr,
    port: u16,
}

/// Keeps the DNS-SD daemon and event consumer alive for the lifetime of the
/// web server. Dropping it unregisters the local service and stops browsing.
pub struct DiscoveryHandle {
    daemon: ServiceDaemon,
    browser: JoinHandle<()>,
}

impl Drop for DiscoveryHandle {
    fn drop(&mut self) {
        self.browser.abort();
        let _ = self.daemon.shutdown();
    }
}

/// Advertises this node and continuously mirrors nearby discovered nodes into
/// the coordinator's existing federation registry. A service record is never
/// trusted on its own: `RemoteMachine::from_discovery` requires HTTPS with the
/// configured private CA before it will transmit the local bearer token.
///
/// # Errors
///
/// Returns an error when discovery is unsafe for the selected bind address,
/// credentials cannot be resolved, or the operating system cannot start mDNS.
pub fn start(
    config: &Config,
    bind: SocketAddr,
    control: ControlPlane,
    token: Secret,
) -> Result<DiscoveryHandle> {
    if bind.ip().is_loopback() || !bind.ip().is_unspecified() {
        bail!(
            "[discovery] requires an all-interface IPv4 bind; run `atmux web --bind 0.0.0.0:7345 --allow-remote`"
        );
    }
    let tls = config
        .node
        .tls
        .as_ref()
        .context("[discovery] requires [node.tls]")?
        .clone();
    let daemon = ServiceDaemon::new().context("failed to start the LAN discovery daemon")?;
    let label = config.node_label();
    let instance = instance_name(&config.node.id);
    let host = format!("atmux-{}.local.", config.node.id);
    let properties = [
        ("id", config.node.id.as_str()),
        ("label", label.as_str()),
        ("version", env!("CARGO_PKG_VERSION")),
    ];
    let service = ServiceInfo::new(
        SERVICE_TYPE,
        &instance,
        &host,
        "",
        bind.port(),
        &properties[..],
    )
    .context("failed to build the LAN discovery advertisement")?
    .enable_addr_auto();
    daemon
        .register(service)
        .context("failed to advertise this atmux node on the LAN")?;
    let receiver = daemon
        .browse(SERVICE_TYPE)
        .context("failed to browse for nearby atmux nodes")?;
    let browser = tokio::spawn(async move {
        let mut services = HashMap::<String, (DiscoveredNode, Instant)>::new();
        let mut active = BTreeSet::new();
        let mut expiry = tokio::time::interval(Duration::from_secs(60));
        expiry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                event = receiver.recv_async() => {
                    let Ok(event) = event else { break };
                    match event {
                        ServiceEvent::ServiceResolved(service) => {
                            if let Some(node) = node_from_resolved(&service) {
                                remember_service(&mut services, node, Instant::now());
                                reconcile(&control, &services, &mut active, &token, &tls);
                            }
                        }
                        ServiceEvent::ServiceRemoved(_, fullname)
                            if services.remove(&fullname).is_some() =>
                        {
                            reconcile(&control, &services, &mut active, &token, &tls);
                        }
                        _ => {}
                    }
                }
                _ = expiry.tick() => {
                    expire_services(&mut services, Instant::now());
                    reconcile(&control, &services, &mut active, &token, &tls);
                }
            }
        }
    });
    Ok(DiscoveryHandle { daemon, browser })
}

fn reconcile(
    control: &ControlPlane,
    services: &HashMap<String, (DiscoveredNode, Instant)>,
    active: &mut BTreeSet<String>,
    token: &Secret,
    tls: &crate::config::TlsConfig,
) {
    // A node id must be unique on the LAN. If a stale or hostile duplicate is
    // announced, choose deterministically so one multicast packet cannot make
    // the coordinator flap between endpoints.
    let mut wanted = BTreeMap::<String, DiscoveredNode>::new();
    for (node, _) in services.values() {
        wanted
            .entry(node.id.clone())
            .and_modify(|current| {
                if node.fullname < current.fullname {
                    *current = node.clone();
                }
            })
            .or_insert_with(|| node.clone());
    }
    while wanted.len() > MAX_DISCOVERED_MACHINES {
        wanted.pop_last();
    }
    let wanted_ids = wanted.keys().cloned().collect::<BTreeSet<_>>();
    for id in active.difference(&wanted_ids).cloned().collect::<Vec<_>>() {
        control.remove_discovered_machine(&id);
    }
    for node in wanted.values() {
        if let Ok(machine) = RemoteMachine::from_discovery(
            node.id.clone(),
            node.label.clone(),
            node.address,
            node.port,
            Some(token.clone()),
            tls,
        ) {
            control.upsert_discovered_machine(machine);
        }
    }
    *active = wanted.into_keys().collect();
}

fn remember_service(
    services: &mut HashMap<String, (DiscoveredNode, Instant)>,
    node: DiscoveredNode,
    now: Instant,
) {
    services.insert(node.fullname.clone(), (node, now));
    while services.len() > MAX_DISCOVERED_SERVICES {
        let Some(fullname) = services.keys().max().cloned() else {
            break;
        };
        services.remove(&fullname);
    }
}

fn expire_services(services: &mut HashMap<String, (DiscoveredNode, Instant)>, now: Instant) {
    services
        .retain(|_, (_, seen)| now.saturating_duration_since(*seen) <= DISCOVERED_SERVICE_MAX_AGE);
}

fn node_from_resolved(service: &ResolvedService) -> Option<DiscoveredNode> {
    let id = service.get_property_val_str("id")?.trim().to_owned();
    validate_machine_id(&id).ok()?;
    let label = service
        .get_property_val_str("label")
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .unwrap_or(&id)
        .to_owned();
    validate_machine_label(&label).ok()?;
    let address = service
        .get_addresses_v4()
        .into_iter()
        .filter_map(|address| lan_address_rank(address).map(|rank| (rank, address)))
        .min_by_key(|(rank, address)| (*rank, u32::from(*address)))?
        .1;
    Some(DiscoveredNode {
        fullname: service.get_fullname().to_owned(),
        id,
        label,
        address,
        port: service.get_port(),
    })
}

/// Ranks usable local addresses. A host can advertise bridge-only link-local
/// interfaces alongside its real LAN interface (notably on macOS), so never
/// select a `169.254/16` address while an RFC1918 address is available.
fn lan_address_rank(address: Ipv4Addr) -> Option<u8> {
    let octets = address.octets();
    if address.is_private() {
        Some(0)
    } else if octets[0] == 100 && (64..=127).contains(&octets[1]) {
        Some(1)
    } else if address.is_link_local() {
        Some(2)
    } else {
        None
    }
}

fn instance_name(id: &str) -> String {
    let mut hasher = DefaultHasher::new();
    id.hash(&mut hasher);
    format!("atmux-{:016x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolved(properties: &[(&str, &str)], address: &str) -> ResolvedService {
        ServiceInfo::new(
            SERVICE_TYPE,
            "test-node",
            "test-node.local.",
            address,
            7345,
            properties,
        )
        .unwrap()
        .as_resolved_service()
    }

    #[test]
    fn accepts_a_valid_private_lan_advertisement() {
        let service = resolved(&[("id", "gpu-box"), ("label", "GPU box")], "192.168.1.8");
        let node = node_from_resolved(&service).unwrap();
        assert_eq!(node.id, "gpu-box");
        assert_eq!(node.label, "GPU box");
        assert_eq!(node.address, "192.168.1.8".parse::<Ipv4Addr>().unwrap());
    }

    #[test]
    fn ignores_invalid_ids_and_non_lan_addresses() {
        assert!(node_from_resolved(&resolved(&[("id", "GPU")], "192.168.1.8")).is_none());
        assert!(node_from_resolved(&resolved(&[("id", "gpu")], "8.8.8.8")).is_none());
    }

    #[test]
    fn prefers_a_routable_private_address_over_a_link_local_bridge() {
        let node = node_from_resolved(&resolved(
            &[("id", "mac"), ("label", "Mac")],
            "169.254.12.8,192.168.1.8",
        ))
        .unwrap();
        assert_eq!(node.address, "192.168.1.8".parse::<Ipv4Addr>().unwrap());
    }

    #[test]
    fn instance_names_are_short_and_stable() {
        assert_eq!(instance_name("workstation"), instance_name("workstation"));
        assert_ne!(instance_name("workstation"), instance_name("gpu-box"));
        assert!(instance_name(&"a".repeat(32)).len() <= 30);
    }

    #[test]
    fn discovery_candidates_are_bounded_and_expire() {
        let now = Instant::now();
        let mut services = HashMap::new();
        for index in 0..(MAX_DISCOVERED_SERVICES + 20) {
            let fullname = format!("atmux-{index:03}.{SERVICE_TYPE}");
            remember_service(
                &mut services,
                DiscoveredNode {
                    fullname,
                    id: format!("node-{index:03}"),
                    label: format!("Node {index}"),
                    address: Ipv4Addr::new(192, 168, 1, 8),
                    port: 7345,
                },
                now,
            );
        }
        assert_eq!(services.len(), MAX_DISCOVERED_SERVICES);
        expire_services(
            &mut services,
            now + DISCOVERED_SERVICE_MAX_AGE + Duration::from_secs(1),
        );
        assert!(services.is_empty());
    }
}
