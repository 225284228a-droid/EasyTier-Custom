//! Host VPN capture routes are installed with native socket bypass.
#[path = "vpn_route_manager.rs"]
mod manager;
pub use manager::{
    VpnCaptureClaims, VpnRouteManagerContext, VpnRouteStatusCell, run_vpn_route_manager,
};
#[cfg(all(test, feature = "tun"))]
pub(crate) use manager::{finish_shared_capture_cleanup, reconcile_shared_capture};
