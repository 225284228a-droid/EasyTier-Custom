//! Host VPN capture routes are installed with native socket bypass.
#[path = "vpn_route_manager.rs"]
mod manager;
pub use manager::{VpnRouteManagerContext, VpnRouteStatusCell, run_vpn_route_manager};
