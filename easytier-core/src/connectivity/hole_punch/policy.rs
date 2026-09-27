use crate::config::P2pPolicyFlags;
use crate::proto::common::PeerFeatureFlag;

pub use crate::foundation::backoff::BackOff;

pub fn should_try_p2p_with_peer(
    feature_flag: Option<&PeerFeatureFlag>,
    allow_public_server: bool,
    local_disable_p2p: bool,
    local_need_p2p: bool,
) -> bool {
    feature_flag
        .map(|flag| {
            (allow_public_server || !flag.is_public_server)
                && (!local_disable_p2p || flag.need_p2p)
                && (!flag.disable_p2p || local_need_p2p)
        })
        .unwrap_or(!local_disable_p2p)
}

pub fn should_background_p2p_with_peer(
    feature_flag: Option<&PeerFeatureFlag>,
    allow_public_server: bool,
    lazy_p2p: bool,
    local_disable_p2p: bool,
    local_need_p2p: bool,
) -> bool {
    should_try_p2p_with_peer(
        feature_flag,
        allow_public_server,
        local_disable_p2p,
        local_need_p2p,
    ) && (!lazy_p2p || feature_flag.map(|flag| flag.need_p2p).unwrap_or(false))
}

/// Whether an inbound hole-punch request from a peer advertising
/// `caller_need_p2p` is accepted when the local node disabled P2P. The
/// `need_p2p` flag stays the only exception; peers without the flag are
/// rejected, mirroring the initiator-side `unwrap_or(!local_disable_p2p)`
/// strictness from the disabled node's point of view.
pub fn should_accept_inbound_punch(local_disable_p2p: bool, caller_need_p2p: bool) -> bool {
    !local_disable_p2p || caller_need_p2p
}

/// Shared per-peer admission gate for the automatic P2P engines (direct
/// connector, TCP and UDP hole punching). A peer qualifies when background
/// P2P is allowed unconditionally, or when on-demand P2P is allowed and the
/// peer either has recent traffic or still lacks an acceptable direct
/// connection (`already_connected` = a connection satisfying the engine's
/// transport policy exists).
pub fn p2p_engine_gate(
    feature_flag: Option<&PeerFeatureFlag>,
    allow_public_server: bool,
    policy: &P2pPolicyFlags,
    has_recent_traffic: bool,
    has_direct_connection: bool,
    already_connected: bool,
) -> bool {
    let static_allowed = should_background_p2p_with_peer(
        feature_flag,
        allow_public_server,
        policy.lazy_p2p,
        policy.disable_p2p,
        policy.need_p2p,
    );
    let dynamic_allowed = should_try_p2p_with_peer(
        feature_flag,
        allow_public_server,
        policy.disable_p2p,
        policy.need_p2p,
    );
    static_allowed
        || (dynamic_allowed
            && (has_recent_traffic || (has_direct_connection && !already_connected)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lazy_background_p2p_requires_need_p2p() {
        let no_need_p2p = PeerFeatureFlag {
            need_p2p: false,
            ..Default::default()
        };
        let need_p2p = PeerFeatureFlag {
            need_p2p: true,
            ..Default::default()
        };

        assert!(should_background_p2p_with_peer(
            Some(&no_need_p2p),
            false,
            false,
            false,
            false
        ));
        assert!(!should_background_p2p_with_peer(
            Some(&no_need_p2p),
            false,
            true,
            false,
            false
        ));
        assert!(should_background_p2p_with_peer(
            Some(&need_p2p),
            false,
            true,
            false,
            false
        ));
    }

    #[test]
    fn p2p_policy_respects_public_server_setting() {
        let public_server = PeerFeatureFlag {
            is_public_server: true,
            ..Default::default()
        };

        assert!(!should_try_p2p_with_peer(
            Some(&public_server),
            false,
            false,
            false
        ));
        assert!(should_try_p2p_with_peer(
            Some(&public_server),
            true,
            false,
            false
        ));
        assert!(!should_background_p2p_with_peer(
            Some(&public_server),
            false,
            false,
            false,
            false
        ));
        assert!(should_background_p2p_with_peer(
            Some(&public_server),
            true,
            false,
            false,
            false
        ));
    }

    #[test]
    fn disable_p2p_only_allows_need_p2p_exceptions() {
        let normal_peer = PeerFeatureFlag::default();
        let need_peer = PeerFeatureFlag {
            need_p2p: true,
            ..Default::default()
        };
        let disable_peer = PeerFeatureFlag {
            disable_p2p: true,
            ..Default::default()
        };
        let disable_need_peer = PeerFeatureFlag {
            disable_p2p: true,
            need_p2p: true,
            ..Default::default()
        };

        assert!(should_try_p2p_with_peer(
            Some(&normal_peer),
            false,
            false,
            false
        ));
        assert!(should_try_p2p_with_peer(None, false, false, false));
        assert!(!should_try_p2p_with_peer(None, false, true, false));
        assert!(!should_try_p2p_with_peer(
            Some(&normal_peer),
            false,
            true,
            false
        ));
        assert!(should_try_p2p_with_peer(
            Some(&need_peer),
            false,
            true,
            false
        ));
        assert!(!should_try_p2p_with_peer(
            Some(&disable_peer),
            false,
            false,
            false
        ));
        assert!(should_try_p2p_with_peer(
            Some(&disable_peer),
            false,
            false,
            true
        ));
        assert!(should_try_p2p_with_peer(
            Some(&disable_need_peer),
            false,
            true,
            true
        ));
        assert!(!should_try_p2p_with_peer(
            Some(&disable_need_peer),
            false,
            true,
            false
        ));
    }

    #[test]
    fn inbound_punch_respects_local_disable_with_need_p2p_exception() {
        // When P2P is enabled locally every inbound punch is accepted.
        assert!(should_accept_inbound_punch(false, false));
        assert!(should_accept_inbound_punch(false, true));

        // A disabled node rejects normal peers and unknown callers, but
        // still serves peers that explicitly declare need_p2p.
        assert!(!should_accept_inbound_punch(true, false));
        assert!(should_accept_inbound_punch(true, true));
    }

    #[test]
    fn engine_gate_admits_background_or_on_demand_peers() {
        let mut policy = P2pPolicyFlags::default();
        // Without lazy P2P the background allowance admits unconditionally,
        // even when a policy-satisfying connection already exists (the
        // caller decides what to do with that combination).
        assert!(p2p_engine_gate(None, false, &policy, false, false, true));

        // lazy_p2p removes the background allowance...
        policy.lazy_p2p = true;
        assert!(!p2p_engine_gate(None, false, &policy, false, false, true));
        // ...but on-demand P2P still admits recent traffic, and a peer with
        // a direct connection only while the policy is not satisfied yet.
        assert!(p2p_engine_gate(None, false, &policy, true, false, true));
        assert!(p2p_engine_gate(None, false, &policy, false, true, false));
        assert!(!p2p_engine_gate(None, false, &policy, false, true, true));
    }
}
