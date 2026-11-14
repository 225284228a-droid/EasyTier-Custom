//! Custom additions to [`PeerConn`] kept out of the upstream-shaped module.

use std::sync::Arc;

use super::super::{cleanup_policy::CleanupPolicyPair, parallel};
use super::{PeerConn, PeerConnectionOrigin};
use crate::{packet::ZCPacket, tunnel::TunnelError};

impl PeerConn {
    pub(crate) fn origin(&self) -> PeerConnectionOrigin {
        self.origin
    }

    pub(crate) fn cleanup_policy(&self) -> &CleanupPolicyPair {
        &self.cleanup_policy
    }

    pub(crate) fn is_manual(&self) -> bool {
        self.cleanup_policy.is_manual()
    }

    pub(super) fn handshake_features(&self) -> Vec<String> {
        #[cfg(test)]
        if let Some(features) = &self.handshake_features_override {
            return features.clone();
        }
        let mut features = self.local_features();
        features.push(self.local_parallel_offer().feature());
        features.push(format!("p2p-tx-v1:{}", self.parallel_tx_epoch));
        if let Some(member) = &self.parallel_member {
            features.push(member.feature());
        }
        if let Some(feature) = self.cleanup_policy.feature() {
            features.push(feature);
        }
        features
    }

    #[cfg(test)]
    pub(crate) fn override_handshake_features(&mut self, features: Vec<String>) {
        self.handshake_features_override = Some(features);
    }

    /// Whether a verified, preferred P2P connection may replace this one.
    pub(crate) fn can_retire_as_redundant(&self) -> bool {
        self.cleanup_policy.can_retire_automatic()
    }

    pub(crate) fn set_parallel_receiver(&mut self, receiver: Arc<parallel::ParallelReceiver>) {
        receiver.register_lane(
            self.conn_id,
            self.tunnel_info
                .as_ref()
                .is_some_and(|tunnel| parallel::ordered_transport(&tunnel.tunnel_type)),
        );
        self.parallel_receiver = Some(receiver);
    }

    pub(crate) fn set_parallel_policy(&mut self, policy: Arc<parallel::ParallelPolicy>) {
        policy.handshake(parallel::remote_offer(
            &self.info.as_ref().unwrap().features,
        ));
        self.parallel_policy = Some(policy);
    }

    pub(crate) fn parallel_can_trim(&self) -> bool {
        self.parallel_policy
            .as_ref()
            .is_some_and(|policy| policy.can_trim(self.local_parallel_offer()))
    }

    pub(crate) fn set_parallel_foreign(&mut self) {
        self.parallel_foreign = true;
    }

    pub(crate) fn local_parallel_offer(&self) -> parallel::ParallelOffer {
        let mut offer = parallel::ParallelOffer::local(self.context.as_ref());
        if self.parallel_foreign {
            offer.limit = offer.limit.min(
                self.context
                    .max_direct_conns_per_peer_in_foreign_network()
                    .clamp(1, 16) as u32,
            );
        }
        offer
    }

    pub(crate) fn parallel_connection_limit(&self) -> u32 {
        self.local_parallel_offer().limit.min(
            self.parallel_policy
                .as_ref()
                .and_then(|policy| policy.remote())
                .or_else(|| parallel::remote_offer(&self.info.as_ref().unwrap().features))
                .map_or(16, |offer| offer.limit),
        )
    }

    pub(crate) fn parallel_member(&self) -> Option<&parallel::ParallelMemberToken> {
        self.parallel_member.as_ref()
    }

    pub(crate) fn parallel_tx_epoch(&self) -> uuid::Uuid {
        self.parallel_tx_epoch
    }

    pub(crate) fn parallel_target(&self) -> u32 {
        let features = &self.info.as_ref().unwrap().features;
        let Some(handshake_offer) = parallel::remote_offer(features) else {
            return 1;
        };
        if parallel::tx_epoch(features).is_none() {
            return 1;
        }
        if self.is_attached()
            || self.tunnel_info.as_ref().is_none_or(|t| {
                matches!(
                    t.tunnel_type.rsplit('-').next(),
                    Some("ring" | "virtualrelay")
                )
            })
        {
            return 1;
        }
        if self.parallel_member.is_some()
            && parallel::ParallelMemberToken::from_features(&self.info.as_ref().unwrap().features)
                .as_ref()
                != self.parallel_member.as_ref()
        {
            return 1;
        }
        let remote = self
            .parallel_policy
            .as_ref()
            .and_then(|policy| policy.remote())
            .unwrap_or(handshake_offer);
        self.local_parallel_offer().target(remote)
    }

    /// Upload rate measured from what the peer reported receiving; zero
    /// until this connection has carried enough recent traffic.
    pub(crate) fn delivered_tx_bps(&self) -> u64 {
        self.throughput.delivered_tx_bps()
    }

    pub(crate) fn try_send_owned(
        &self,
        msg: ZCPacket,
    ) -> Result<(), tokio::sync::mpsc::error::TrySendError<ZCPacket>> {
        self.sink.try_send_owned(msg)
    }

    pub(crate) async fn reserve_send(
        &self,
    ) -> Result<tokio::sync::mpsc::OwnedPermit<ZCPacket>, TunnelError> {
        self.sink.reserve_owned().await
    }
}

#[cfg(test)]
pub(super) fn cleanup_handshake_context(
    key: u8,
    secure: bool,
    flags: crate::proto::common::Flags,
) -> crate::peers::context::ArcPeerContext {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;

    let mut context =
        crate::peers::test_support::NoopPeerContext::new(crate::peers::context::NetworkIdentity {
            network_name: "cleanup-handshake".to_owned(),
            network_secret: Some("cleanup-secret".to_owned()),
            network_secret_digest: None,
        })
        .with_flags(flags);
    if secure {
        let private = x25519_dalek::StaticSecret::from([key; 32]);
        let public = x25519_dalek::PublicKey::from(&private);
        context = context.with_secure_mode(crate::proto::common::SecureModeConfig {
            enabled: true,
            local_private_key: Some(BASE64_STANDARD.encode(private.as_bytes())),
            local_public_key: Some(BASE64_STANDARD.encode(public.as_bytes())),
        });
    }
    std::sync::Arc::new(context)
}
