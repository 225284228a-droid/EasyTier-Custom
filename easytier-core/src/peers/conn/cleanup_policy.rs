use crate::{
    config::{P2pPolicyFlags, PeerDisguiseP2pFlags, normalized_p2p_protocol, p2p_protocol_rank},
    peers::PeerConnectionOrigin,
    proto::common::FlagsInConfig,
};

const FEATURE_FAMILY: &str = "p2p-cleanup-";
// v1 ranks raw transports first when disguise is not initiated. Never mix its
// cleanup decisions with v2's established-connection ordering.
const FEATURE_PREFIX: &str = "p2p-cleanup-v2:";

/// The ordering inputs advertised for one connection, independent of whether
/// this endpoint has enabled cleanup. Keeping this immutable lets a live config
/// change cancel cleanup until both endpoints have handshaken the new policy.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CleanupPolicySnapshot {
    protocol: String,
    prefer: bool,
    only: bool,
    disabled: bool,
}

impl CleanupPolicySnapshot {
    fn from_flags(flags: &FlagsInConfig) -> Self {
        Self {
            protocol: normalized_p2p_protocol(&flags.default_protocol),
            prefer: flags.prefer_wss_http3_for_p2p,
            only: flags.only_use_wss_http3_for_hole_punching,
            disabled: flags.disable_wss_http3_for_p2p,
        }
    }

    fn valid_protocol(protocol: &str) -> bool {
        let mut bytes = protocol.bytes();
        bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
            && bytes.all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"+.-".contains(&byte)
            })
    }

    fn peer_disguise_flags(&self) -> PeerDisguiseP2pFlags {
        PeerDisguiseP2pFlags {
            prefer_wss_http3_for_p2p: self.prefer,
            only_use_wss_http3_for_p2p: self.only,
            disable_wss_http3_for_p2p: self.disabled,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CleanupOffer {
    policy: CleanupPolicySnapshot,
    enabled: bool,
    origin: PeerConnectionOrigin,
    conn_id: uuid::Uuid,
}

impl CleanupOffer {
    fn feature(&self) -> Option<String> {
        CleanupPolicySnapshot::valid_protocol(&self.policy.protocol).then(|| {
            let origin = match self.origin {
                PeerConnectionOrigin::Manual => "manual",
                PeerConnectionOrigin::Direct => "direct",
                PeerConnectionOrigin::Listener => "listener",
                PeerConnectionOrigin::TcpHolePunch => "tcp-punch",
                PeerConnectionOrigin::UdpHolePunch => "udp-punch",
                PeerConnectionOrigin::Attached => "attached",
            };
            format!(
                "{FEATURE_PREFIX}{}:{}:{}:{}:{}:{origin}:{}",
                self.policy.protocol,
                u8::from(self.policy.prefer),
                u8::from(self.policy.only),
                u8::from(self.policy.disabled),
                u8::from(self.enabled),
                self.conn_id,
            )
        })
    }

    fn from_features(features: &[String]) -> Option<Self> {
        let mut policy = None;
        for feature in features {
            if !feature.starts_with(FEATURE_FAMILY) {
                continue;
            }
            // Unknown versions, malformed tokens and even identical duplicate
            // offers cannot establish a single shared ordering. They do not
            // reject the peer handshake; only cleanup is disabled.
            if policy.is_some() {
                return None;
            }
            let mut fields = feature.strip_prefix(FEATURE_PREFIX)?.split(':');
            let protocol = fields.next()?;
            if !CleanupPolicySnapshot::valid_protocol(protocol) {
                return None;
            }
            let mut flag = || match fields.next()? {
                "0" => Some(false),
                "1" => Some(true),
                _ => None,
            };
            let prefer = flag()?;
            let only = flag()?;
            let disabled = flag()?;
            let enabled = flag()?;
            let origin = match fields.next()? {
                "manual" => PeerConnectionOrigin::Manual,
                "direct" => PeerConnectionOrigin::Direct,
                "listener" => PeerConnectionOrigin::Listener,
                "tcp-punch" => PeerConnectionOrigin::TcpHolePunch,
                "udp-punch" => PeerConnectionOrigin::UdpHolePunch,
                "attached" => PeerConnectionOrigin::Attached,
                _ => return None,
            };
            let conn_id = fields.next()?.parse::<uuid::Uuid>().ok()?;
            if conn_id.is_nil() {
                return None;
            }
            if fields.next().is_some() {
                return None;
            }
            policy = Some(Self {
                policy: CleanupPolicySnapshot {
                    protocol: protocol.to_owned(),
                    prefer,
                    only,
                    disabled,
                },
                enabled,
                origin,
                conn_id,
            });
        }
        policy
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CleanupPolicyPair {
    local: CleanupOffer,
    remote: Option<CleanupOffer>,
}

impl CleanupPolicyPair {
    pub(crate) fn new(
        flags: &FlagsInConfig,
        origin: PeerConnectionOrigin,
        conn_id: uuid::Uuid,
    ) -> Self {
        Self {
            local: CleanupOffer {
                policy: CleanupPolicySnapshot::from_flags(flags),
                enabled: flags.close_redundant_conns_when_disguised,
                origin,
                conn_id,
            },
            remote: None,
        }
    }

    pub(crate) fn feature(&self) -> Option<String> {
        self.local.feature()
    }

    pub(crate) fn set_remote_features(&mut self, features: &[String]) {
        self.remote = CleanupOffer::from_features(features);
    }

    pub(crate) fn matches_current_flags(&self, flags: &FlagsInConfig) -> bool {
        self.remote.is_some() && self.local.policy == CleanupPolicySnapshot::from_flags(flags)
    }

    /// Connection IDs and origins differ across paths; only the immutable
    /// ordering inputs must match before one path can replace another.
    pub(crate) fn same_ordering_as(&self, other: &Self) -> bool {
        self.local.policy == other.local.policy
            && self.remote.as_ref().map(|offer| &offer.policy)
                == other.remote.as_ref().map(|offer| &offer.policy)
            && self.remote.is_some()
    }

    pub(crate) fn is_manual(&self) -> bool {
        self.local.origin == PeerConnectionOrigin::Manual
            || self
                .remote
                .as_ref()
                .is_some_and(|offer| offer.origin == PeerConnectionOrigin::Manual)
    }

    pub(crate) fn remote_cleanup_enabled(&self) -> Option<bool> {
        self.remote.as_ref().map(|offer| offer.enabled)
    }

    pub(crate) fn shared_connection_key(&self) -> Option<(uuid::Uuid, uuid::Uuid)> {
        let remote = self.remote.as_ref()?.conn_id;
        Some((
            self.local.conn_id.min(remote),
            self.local.conn_id.max(remote),
        ))
    }

    pub(crate) fn can_retire_automatic(&self) -> bool {
        let Some(remote) = self.remote.as_ref() else {
            return false;
        };
        !self.is_manual()
            && ![self.local.origin, remote.origin].contains(&PeerConnectionOrigin::Attached)
            && [self.local.origin, remote.origin]
                .into_iter()
                .any(|origin| {
                    matches!(
                        origin,
                        PeerConnectionOrigin::Direct
                            | PeerConnectionOrigin::TcpHolePunch
                            | PeerConnectionOrigin::UdpHolePunch
                    )
                })
    }

    /// Peers may choose different TCP/UDP preferences. Retire an automatic
    /// path only when both orderings rank its replacement strictly higher.
    pub(crate) fn both_prefer_transport(&self, replacement: &str, candidate: &str) -> bool {
        let Some(remote) = self.remote.as_ref() else {
            return false;
        };
        [&self.local.policy, &remote.policy]
            .into_iter()
            .all(|policy| {
                p2p_protocol_rank(&policy.protocol, !policy.disabled, replacement)
                    < p2p_protocol_rank(&policy.protocol, !policy.disabled, candidate)
            })
    }

    pub(crate) fn agreed_disguise(&self, current_flags: &FlagsInConfig) -> Option<bool> {
        let remote = self.remote.as_ref()?;
        if !self.matches_current_flags(current_flags) {
            return None;
        }
        let local = P2pPolicyFlags {
            prefer_wss_http3_for_p2p: self.local.policy.prefer,
            only_use_wss_http3_for_hole_punching: self.local.policy.only,
            disable_wss_http3_for_p2p: self.local.policy.disabled,
            ..Default::default()
        };
        Some(local.use_wss_http3_with_peer(&remote.policy.peer_disguise_flags()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(flags: &FlagsInConfig) -> CleanupPolicyPair {
        CleanupPolicyPair::new(flags, PeerConnectionOrigin::Direct, uuid::Uuid::new_v4())
    }

    #[test]
    fn cleanup_offer_requires_one_complete_known_version() {
        for features in [
            vec![],
            vec!["liveness-echo-v1"],
            vec!["p2p-cleanup-v1:tcp:0:0:0"],
            vec!["p2p-cleanup-v3:tcp:0:0:0"],
            vec!["p2p-cleanup-v1:TCP:0:0:0"],
            vec!["p2p-cleanup-v1::0:0:0"],
            vec!["p2p-cleanup-v1:tcp:2:0:0"],
            vec!["p2p-cleanup-v1:tcp:0:0"],
            vec!["p2p-cleanup-v1:tcp:0:0:0:extra"],
            vec!["p2p-cleanup-v1:tcp:0:0:0", "p2p-cleanup-v1:tcp:0:0:0"],
            vec!["p2p-cleanup-v1:tcp:0:0:0", "p2p-cleanup-v1:udp:0:0:0"],
            vec!["p2p-cleanup-v1:tcp:0:0:0", "p2p-cleanup-v2:tcp:0:0:0"],
        ] {
            let features = features.into_iter().map(str::to_owned).collect::<Vec<_>>();
            assert!(
                CleanupOffer::from_features(&features).is_none(),
                "{features:?}"
            );
        }
        let feature = policy(&FlagsInConfig {
            default_protocol: "tcp".to_owned(),
            ..Default::default()
        })
        .feature()
        .unwrap();
        for invalid in [
            feature.replace(":tcp:", ":TCP:"),
            feature.replace(":tcp:", "::"),
            feature.replace(":0:0:0:", ":2:0:0:"),
            feature.replace(":direct:", ":unknown:"),
            format!("{feature}:extra"),
            "p2p-cleanup-v2:tcp:0:0:0:0:direct:not-a-uuid".to_owned(),
            "p2p-cleanup-v2:tcp:0:0:0:0:direct:00000000-0000-0000-0000-000000000000".to_owned(),
        ] {
            assert!(
                CleanupOffer::from_features(&[invalid.clone()]).is_none(),
                "{invalid}"
            );
        }
        assert!(CleanupOffer::from_features(&[feature.clone(), feature.clone()]).is_none());
        assert!(
            CleanupOffer::from_features(&[feature.clone(), "p2p-cleanup-v1:tcp:0:0:0".to_owned()])
                .is_none()
        );
        let features = vec!["future-other-feature".to_owned(), feature];
        assert_eq!(
            CleanupOffer::from_features(&features).unwrap().feature(),
            Some(features[1].clone())
        );
    }

    #[test]
    fn cleanup_order_is_symmetric_and_respects_both_protocol_preferences() {
        for protocol in ["", " tcp ", "UDP"] {
            for (a_prefer, a_only, a_disabled) in [
                (false, false, false),
                (true, false, false),
                (false, true, false),
                (true, true, true),
            ] {
                for (b_prefer, b_only, b_disabled) in [
                    (false, false, false),
                    (true, false, false),
                    (false, true, false),
                    (true, true, true),
                ] {
                    let a_flags = FlagsInConfig {
                        default_protocol: protocol.to_owned(),
                        prefer_wss_http3_for_p2p: a_prefer,
                        only_use_wss_http3_for_hole_punching: a_only,
                        disable_wss_http3_for_p2p: a_disabled,
                        ..Default::default()
                    };
                    let b_flags = FlagsInConfig {
                        default_protocol: normalized_p2p_protocol(protocol),
                        prefer_wss_http3_for_p2p: b_prefer,
                        only_use_wss_http3_for_hole_punching: b_only,
                        disable_wss_http3_for_p2p: b_disabled,
                        ..Default::default()
                    };
                    let mut a = policy(&a_flags);
                    let mut b = policy(&b_flags);
                    a.set_remote_features(&[b.feature().unwrap()]);
                    b.set_remote_features(&[a.feature().unwrap()]);
                    assert_eq!(a.agreed_disguise(&a_flags), b.agreed_disguise(&b_flags));
                    assert!(a.agreed_disguise(&a_flags).is_some());
                }
            }
        }
        let flags = FlagsInConfig {
            default_protocol: "tcp".to_owned(),
            ..Default::default()
        };
        let mut paired = policy(&flags);
        let remote = policy(&FlagsInConfig {
            default_protocol: "udp".to_owned(),
            ..flags.clone()
        });
        paired.set_remote_features(&[remote.feature().unwrap()]);
        assert_eq!(paired.agreed_disguise(&flags), Some(false));
        assert!(paired.both_prefer_transport("wss", "tcp"));
        assert!(paired.both_prefer_transport("http3", "udp"));
        for (replacement, candidate) in [
            ("tcp", "udp"),
            ("udp", "tcp"),
            ("wss", "http3"),
            ("http3", "wss"),
        ] {
            assert!(!paired.both_prefer_transport(replacement, candidate));
        }
        let changed = FlagsInConfig {
            prefer_wss_http3_for_p2p: true,
            ..flags.clone()
        };
        assert_eq!(paired.agreed_disguise(&changed), None);
    }

    #[test]
    fn manual_origin_and_connection_order_are_shared_without_matching_protocol_preferences() {
        let flags = FlagsInConfig {
            default_protocol: "tcp".to_owned(),
            ..Default::default()
        };
        let mut a =
            CleanupPolicyPair::new(&flags, PeerConnectionOrigin::Manual, uuid::Uuid::new_v4());
        let remote_flags = FlagsInConfig {
            default_protocol: "udp".to_owned(),
            close_redundant_conns_when_disguised: true,
            ..flags.clone()
        };
        let mut b = CleanupPolicyPair::new(
            &remote_flags,
            PeerConnectionOrigin::Listener,
            uuid::Uuid::new_v4(),
        );
        a.set_remote_features(&[b.feature().unwrap()]);
        b.set_remote_features(&[a.feature().unwrap()]);
        assert!(a.is_manual() && b.is_manual());
        assert!(!a.can_retire_automatic() && !b.can_retire_automatic());
        assert_eq!(a.shared_connection_key(), b.shared_connection_key());
        assert_eq!(a.remote_cleanup_enabled(), Some(true));
        assert_eq!(b.remote_cleanup_enabled(), Some(false));
        let mut another =
            CleanupPolicyPair::new(&flags, PeerConnectionOrigin::Direct, uuid::Uuid::new_v4());
        another.set_remote_features(&[b.feature().unwrap()]);
        assert!(a.same_ordering_as(&another));
        assert_ne!(a, another);
    }

    #[test]
    fn explicit_disguise_ban_keeps_its_ordering_and_conflicts_do_not_retire_either_path() {
        let allowed = FlagsInConfig::default();
        let banned = FlagsInConfig {
            disable_wss_http3_for_p2p: true,
            ..allowed.clone()
        };
        let mut local = policy(&banned);
        local.set_remote_features(&[policy(&allowed).feature().unwrap()]);
        assert!(!local.both_prefer_transport("tcp", "wss"));
        assert!(!local.both_prefer_transport("wss", "tcp"));
        local.set_remote_features(&[policy(&banned).feature().unwrap()]);
        assert!(local.both_prefer_transport("tcp", "wss"));
        assert!(!local.both_prefer_transport("wss", "tcp"));
    }
}
