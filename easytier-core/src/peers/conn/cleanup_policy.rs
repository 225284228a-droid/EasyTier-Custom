use crate::{
    config::{P2pPolicyFlags, PeerDisguiseP2pFlags, normalized_p2p_protocol},
    proto::common::FlagsInConfig,
};

const FEATURE_FAMILY: &str = "p2p-cleanup-";
const FEATURE_PREFIX: &str = "p2p-cleanup-v1:";

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

    fn feature(&self) -> Option<String> {
        Self::valid_protocol(&self.protocol).then(|| {
            format!(
                "{FEATURE_PREFIX}{}:{}:{}:{}",
                self.protocol,
                u8::from(self.prefer),
                u8::from(self.only),
                u8::from(self.disabled),
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
            if !Self::valid_protocol(protocol) {
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
            if fields.next().is_some() {
                return None;
            }
            policy = Some(Self {
                protocol: protocol.to_owned(),
                prefer,
                only,
                disabled,
            });
        }
        policy
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
pub(crate) struct CleanupPolicyPair {
    local: CleanupPolicySnapshot,
    remote: Option<CleanupPolicySnapshot>,
}

impl CleanupPolicyPair {
    pub(crate) fn new(flags: &FlagsInConfig) -> Self {
        Self {
            local: CleanupPolicySnapshot::from_flags(flags),
            remote: None,
        }
    }

    pub(crate) fn feature(&self) -> Option<String> {
        self.local.feature()
    }

    pub(crate) fn set_remote_features(&mut self, features: &[String]) {
        self.remote = CleanupPolicySnapshot::from_features(features);
    }

    pub(crate) fn agreed_disguise(&self, current_flags: &FlagsInConfig) -> Option<bool> {
        let remote = self.remote.as_ref()?;
        if self.local != CleanupPolicySnapshot::from_flags(current_flags)
            || self.local.protocol != remote.protocol
        {
            return None;
        }
        let local = P2pPolicyFlags {
            prefer_wss_http3_for_p2p: self.local.prefer,
            only_use_wss_http3_for_hole_punching: self.local.only,
            disable_wss_http3_for_p2p: self.local.disabled,
            ..Default::default()
        };
        Some(local.use_wss_http3_with_peer(&remote.peer_disguise_flags()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_offer_requires_one_complete_known_version() {
        for features in [
            vec![],
            vec!["liveness-echo-v1"],
            vec!["p2p-cleanup-v2:tcp:0:0:0"],
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
                CleanupPolicySnapshot::from_features(&features).is_none(),
                "{features:?}"
            );
        }
        let features = vec![
            "future-other-feature".to_owned(),
            "p2p-cleanup-v1:tcp:1:0:0".to_owned(),
        ];
        assert_eq!(
            CleanupPolicySnapshot::from_features(&features)
                .unwrap()
                .feature(),
            Some(features[1].clone())
        );
    }

    #[test]
    fn cleanup_order_is_symmetric_and_protocols_must_agree() {
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
                    let mut a = CleanupPolicyPair::new(&a_flags);
                    let mut b = CleanupPolicyPair::new(&b_flags);
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
        let mut policy = CleanupPolicyPair::new(&flags);
        policy.set_remote_features(&["p2p-cleanup-v1:udp:0:0:0".to_owned()]);
        assert_eq!(policy.agreed_disguise(&flags), None);
        policy.set_remote_features(&["p2p-cleanup-v1:tcp:0:0:0".to_owned()]);
        let changed = FlagsInConfig {
            prefer_wss_http3_for_p2p: true,
            ..flags.clone()
        };
        assert_eq!(policy.agreed_disguise(&changed), None);
    }
}
