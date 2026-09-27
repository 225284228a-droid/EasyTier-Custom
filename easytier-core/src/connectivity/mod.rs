//! Portable connection orchestration.

use std::fmt::Debug;
use std::net::Ipv6Addr;

use url::Url;

pub mod composite;
pub mod direct;
pub mod hole_punch;
// Kept public: the host-driven adapter chain is WASI-only production code
// (cfg(target_os = "wasi")), so crate-private visibility would surface
// dead-code warnings on host builds for code that is live on WASI.
pub mod connector_host;
pub mod manual;
pub mod protocol;
pub mod stun;
pub mod transport;

/// Whether an IPv6 address is a usable public dialing/listening candidate:
/// it excludes loopback, unspecified, unique-local, link-local and multicast
/// ranges. Callers layer their own extra conditions (managed addresses,
/// testing overrides) on top of this base predicate.
pub(crate) fn is_public_ipv6_candidate(ip: Ipv6Addr) -> bool {
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_unique_local()
        || ip.is_unicast_link_local()
        || ip.is_multicast())
}

/// Supplies the URLs of the instance's currently running listeners.
///
/// The listener layer's running-listener registry implements this seam.
/// Connectors use it to avoid dialing addresses that would hairpin back
/// into one of their own listeners, so connectivity depends on this narrow
/// query rather than on the listener module's concrete registry type.
pub trait LocalListenerUrls: Debug + Send + Sync + 'static {
    fn local_listener_urls(&self) -> Vec<Url>;

    fn udp_http3_listener_urls(&self) -> Vec<Url> {
        Vec::new()
    }
}

/// Empty [`LocalListenerUrls`] for connectors that track no listeners.
#[derive(Debug, Default)]
pub struct NoLocalListeners;

impl LocalListenerUrls for NoLocalListeners {
    fn local_listener_urls(&self) -> Vec<Url> {
        Vec::new()
    }
}
