use std::{any::Any, net::IpAddr, sync::Arc};

use url::Url;

use crate::{
    config::peers::AclWhitelistSnapshot,
    connectivity::manual::ManualConnectorSnapshot,
    foundation::stats::MetricSnapshot,
    peers::{
        conn::peer_conn::PeerConnId,
        credential_manager::{
            CredentialCreateOptions, CredentialInfo, CredentialUpsertOptions, GeneratedCredential,
        },
        peer_manager::PeerSnapshot,
    },
};

use super::{CoreInstance, CoreInstanceHost, CorePacketPlane};

#[cfg(feature = "management-rpc")]
fn projected_vpn_activation_error(
    client_active: bool,
    running: bool,
    no_tun: bool,
    has_virtual_address: bool,
    dhcp_enabled: bool,
    host_error: Option<&str>,
) -> String {
    // These prerequisite reports describe current configuration, rather than
    // an installation failure. Recompute them after hot patches even when no
    // NIC exists to run the route manager, while retaining real setup/cleanup
    // errors reported by that manager.
    if let Some(error) = host_error
        && !matches!(
            error,
            "VPN capture unavailable: no_tun disables the native interface"
                | "VPN capture pending: waiting for DHCP to assign a virtual address"
                | "VPN capture unavailable: no virtual IPv4 or IPv6 address is configured"
        )
    {
        return error.to_owned();
    }
    if !client_active || !running {
        return String::new();
    }
    if no_tun {
        return "VPN capture unavailable: no_tun disables the native interface".to_owned();
    }
    if !has_virtual_address {
        return if dhcp_enabled {
            "VPN capture pending: waiting for DHCP to assign a virtual address"
        } else {
            "VPN capture unavailable: no virtual IPv4 or IPv6 address is configured"
        }
        .to_owned();
    }
    String::new()
}

impl<H> CoreInstance<H>
where
    H: CoreInstanceHost,
{
    pub fn instance_id(&self) -> uuid::Uuid {
        self.peer_manager.instance_id()
    }

    pub fn instance_name(&self) -> &str {
        &self.instance_name
    }

    pub fn add_connector(&self, url: Url) -> anyhow::Result<()> {
        self.manual
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("inbound-only connectivity has no outbound connectors"))?
            .add_connector(url)
    }

    pub fn remove_connector(&self, url: &Url) -> bool {
        self.manual
            .as_ref()
            .is_some_and(|manual| manual.remove_connector(url))
    }

    pub fn clear_connectors(&self) {
        if let Some(manual) = &self.manual {
            manual.clear_connectors();
        }
    }

    pub fn list_connectors(&self) -> Vec<ManualConnectorSnapshot> {
        self.manual
            .as_ref()
            .map(|manual| manual.list_connectors())
            .unwrap_or_default()
    }

    pub fn running_listeners(&self) -> Vec<Url> {
        self.running_listeners.running_listeners()
    }

    pub fn peer_id(&self) -> crate::config::PeerId {
        self.peer_manager.my_peer_id()
    }

    pub fn packet_plane(&self) -> Arc<CorePacketPlane> {
        self.packet_plane.clone()
    }

    /// Recovers the concrete Host runtime Adapter for host-specific integration.
    pub fn runtime_host<T: super::InstanceRuntimeHost>(&self) -> Option<&T> {
        let runtime_host: &dyn Any = self.instance_runtime.as_ref();
        runtime_host.downcast_ref()
    }

    pub fn attach_tun_fd(&self, fd: i32) -> anyhow::Result<()> {
        self.instance_runtime.attach_tun_fd(fd)
    }

    pub async fn attach_tun_fd_and_wait(&self, fd: i32) -> anyhow::Result<()> {
        self.instance_runtime.attach_tun_fd_and_wait(fd).await
    }

    pub async fn attach_tun_fd_and_wait_with_generation(
        &self,
        fd: i32,
        generation: Option<u64>,
    ) -> anyhow::Result<()> {
        self.instance_runtime
            .attach_tun_fd_and_wait_with_generation(fd, generation)
            .await
    }

    pub fn latest_error(&self) -> Option<String> {
        self.latest_error.read().clone()
    }

    pub fn is_ready(&self) -> bool {
        self.state() == super::CoreInstanceState::Running
    }

    pub fn management_events(&self) -> Vec<String> {
        self.instance_runtime.management_events()
    }

    pub fn global_peer_map_snapshot(&self) -> crate::proto::peer_rpc::GetGlobalPeerMapResponse {
        self.peer_center.global_peer_map_snapshot()
    }

    pub async fn peer_snapshots(&self) -> Vec<PeerSnapshot> {
        self.peer_manager.list_peer_snapshots().await
    }

    pub async fn node_snapshot(&self) -> crate::peers::peer_manager::NodeSnapshot {
        let mut snapshot = self
            .peer_manager
            .node_snapshot(self.running_listeners())
            .await;
        if let Some(direct) = &self.direct {
            snapshot.ip_list = direct
                .local_address_observations_with_stun(&snapshot.stun_info)
                .await;
        }
        snapshot
    }

    pub async fn route_snapshots(&self) -> Vec<crate::proto::core_peer::peer::Route> {
        self.peer_manager.list_route_snapshots().await
    }

    pub async fn dump_route(&self) -> String {
        self.peer_manager.dump_route().await
    }

    pub async fn local_public_ipv6_info(
        &self,
    ) -> crate::proto::core_peer::peer::ListPublicIpv6InfoResponse {
        self.peer_manager.local_public_ipv6_info().await
    }

    pub async fn foreign_network_route_infos(
        &self,
    ) -> crate::proto::peer_rpc::RouteForeignNetworkInfos {
        self.peer_manager.foreign_network_route_infos().await
    }

    pub async fn foreign_network_snapshots(
        &self,
        include_trusted_keys: bool,
    ) -> std::collections::HashMap<String, crate::peers::foreign_network::ForeignNetworkEntryInfo>
    {
        self.peer_manager
            .list_foreign_network_infos(include_trusted_keys)
            .await
    }

    pub async fn foreign_network_route_summary(
        &self,
    ) -> crate::proto::peer_rpc::RouteForeignNetworkSummary {
        self.peer_manager.foreign_network_route_summary().await
    }

    pub fn acl_stats(&self) -> crate::proto::acl::AclStats {
        self.peer_manager.acl_stats()
    }

    pub fn acl_whitelist_snapshot(&self) -> AclWhitelistSnapshot {
        let config = self.runtime_config.snapshot();
        AclWhitelistSnapshot::from(&config.services.acl)
    }

    pub fn generate_credential(
        &self,
        options: CredentialCreateOptions,
    ) -> anyhow::Result<GeneratedCredential> {
        if !self.peer_manager.can_manage_credentials() {
            anyhow::bail!("only admin nodes (with network_secret) can generate credentials");
        }
        if options.ttl.is_zero() {
            anyhow::bail!("ttl_seconds must be positive");
        }
        let generated = self
            .peer_manager
            .credential_manager()
            .generate_credential_with_options(options)
            .map_err(anyhow::Error::msg)?;
        self.peer_manager.notify_credential_changed();
        Ok(generated)
    }

    pub fn revoke_credential(&self, credential_id: &str) -> anyhow::Result<bool> {
        if !self.peer_manager.can_manage_credentials() {
            anyhow::bail!("only admin nodes (with network_secret) can revoke credentials");
        }
        let revoked = self
            .peer_manager
            .credential_manager()
            .revoke_credential(credential_id)
            .map_err(anyhow::Error::msg)?;
        if revoked {
            self.peer_manager.notify_credential_changed();
        }
        Ok(revoked)
    }

    pub fn upsert_credential(&self, options: CredentialUpsertOptions) -> anyhow::Result<bool> {
        if !self.peer_manager.can_manage_credentials() {
            anyhow::bail!("only admin nodes (with network_secret) can import credentials");
        }
        let changed = self
            .peer_manager
            .credential_manager()
            .upsert_credential(options)
            .map_err(anyhow::Error::msg)?;
        if changed {
            self.peer_manager.notify_credential_changed();
        }
        Ok(changed)
    }

    pub fn credential_snapshots(&self) -> Vec<CredentialInfo> {
        self.peer_manager.credential_manager().list_credentials()
    }

    #[cfg(feature = "web-client")]
    pub(crate) fn credential_manager(
        &self,
    ) -> Arc<crate::peers::credential_manager::CredentialManager> {
        self.peer_manager.credential_manager()
    }

    #[cfg(feature = "web-client")]
    pub(crate) fn notify_credential_changed(&self) {
        self.peer_manager.notify_credential_changed();
    }

    pub fn metric_snapshots(&self) -> Vec<MetricSnapshot> {
        self.peer_manager.stats_manager().get_all_metrics()
    }

    pub fn prometheus_metrics(&self) -> String {
        self.peer_manager.stats_manager().export_prometheus()
    }

    pub async fn close_peer_conn(
        &self,
        peer_id: crate::config::PeerId,
        conn_id: &PeerConnId,
    ) -> Result<(), crate::peers::error::Error> {
        self.peer_manager.close_peer_conn(peer_id, conn_id).await
    }

    pub async fn update_exit_nodes(&self, exit_nodes: Vec<IpAddr>) {
        self.peer_manager.update_exit_nodes(exit_nodes).await;
    }

    pub async fn update_vpn_config(
        &self,
        vpn: crate::config::vpn::VpnConfig,
    ) -> anyhow::Result<()> {
        self.peer_manager.update_vpn_config(vpn)
    }

    /// VPN status: subnet exits, reachability,
    /// and (when provided by the host) route-manager state.
    #[cfg(feature = "management-rpc")]
    pub async fn vpn_status(&self) -> easytier_proto::api::instance::GetVpnStatusResponse {
        use easytier_proto::api::instance::{GetVpnStatusResponse, VpnRuleStatus};

        self.vpn_engine
            .update_exclusions(self.peer_manager.vpn_route_exclusions().await);
        let config = self.vpn_engine.config();
        let (plan, policy_generation) = self.vpn_engine.route_plan();
        let rules = self.vpn_engine.status();
        let host_state = self
            .vpn_route_state
            .as_ref()
            .and_then(|adapter| adapter.vpn_route_state());
        let runtime = self.runtime_config.snapshot();
        let activation_error = projected_vpn_activation_error(
            config.client_active(),
            self.is_ready(),
            runtime.peer.flags.no_tun,
            runtime.peer.runtime.core.routes.ipv4.is_some()
                || runtime.peer.runtime.core.routes.ipv6.is_some(),
            runtime.services.dhcp_ipv4 || runtime.services.dhcp_ipv6,
            host_state
                .as_ref()
                .and_then(|state| state.activation_error.as_deref()),
        );

        GetVpnStatusResponse {
            enabled: config.client_active(),
            exit_enabled: runtime.services.proxy.enable_exit_node,
            capture_routes: plan
                .capture_routes
                .iter()
                .map(ToString::to_string)
                .collect(),
            policy_generation,
            ipv6_addr: self
                .peer_manager
                .virtual_ipv6()
                .map(|a| a.to_string())
                .unwrap_or_default(),
            routes_active: host_state.as_ref().is_some_and(|state| state.routes_active),
            routes_installed: host_state.as_ref().map_or(0, |s| s.routes_installed as u32),
            activation_error,
            rules: rules
                .into_iter()
                .map(|rule| VpnRuleStatus {
                    cidr: rule.cidr,
                    exit_addr: rule.exit_addr,
                    state: if rule.up { "up".into() } else { "down".into() },
                })
                .collect(),
        }
    }

    pub async fn refresh_acl_groups(&self) {
        self.peer_manager.get_route().refresh_acl_groups().await;
    }
}

#[cfg(all(test, feature = "management-rpc"))]
mod vpn_activation_status_tests {
    use super::projected_vpn_activation_error;

    #[test]
    fn no_tun_hot_patch_reports_new_rules_and_clears_removed_rules() {
        let unavailable = "VPN capture unavailable: no_tun disables the native interface";
        assert_eq!(
            projected_vpn_activation_error(false, true, true, true, false, None),
            ""
        );
        assert_eq!(
            projected_vpn_activation_error(true, true, true, true, false, None),
            unavailable
        );
        assert_eq!(
            projected_vpn_activation_error(false, true, true, true, false, Some(unavailable)),
            ""
        );
    }

    #[test]
    fn address_recovery_and_stop_clear_stale_prerequisite_reports() {
        let waiting = "VPN capture pending: waiting for DHCP to assign a virtual address";
        assert_eq!(
            projected_vpn_activation_error(true, true, false, false, true, None),
            waiting
        );
        assert_eq!(
            projected_vpn_activation_error(true, true, false, true, true, Some(waiting)),
            ""
        );
        assert_eq!(
            projected_vpn_activation_error(true, false, false, false, true, Some(waiting)),
            ""
        );
    }

    #[test]
    fn prerequisite_projection_preserves_actual_setup_and_cleanup_errors() {
        let setup = "could not reserve a VPN bypass mark";
        assert_eq!(
            projected_vpn_activation_error(true, true, true, false, true, Some(setup)),
            setup
        );
        let unsupported = "VPN capture unavailable: platform policy routing is unsupported";
        assert_eq!(
            projected_vpn_activation_error(true, true, false, true, false, Some(unsupported)),
            unsupported
        );
        let cleanup = "could not remove VPN capture routes";
        assert_eq!(
            projected_vpn_activation_error(false, false, true, false, false, Some(cleanup)),
            cleanup
        );
    }
}
