use crate::{
    config::{P2pPolicyFlags, PeerDisguiseP2pFlags, PeerId},
    proto::common::{NatType, PeerFeatureFlag},
};

use super::{
    super::policy::{should_background_p2p_with_peer, should_try_p2p_with_peer},
    UdpNatType,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UdpPunchCandidate {
    pub peer_id: PeerId,
    pub udp_nat_type: NatType,
    pub feature_flag: Option<PeerFeatureFlag>,
    pub has_direct_connection: bool,
    pub has_http3_connection: bool,
    pub has_recent_traffic: bool,
    pub peer_disguise_flags: PeerDisguiseP2pFlags,
}

#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq)]
pub struct UdpPunchTaskInfo {
    pub dst_peer_id: PeerId,
    pub dst_nat_type: UdpNatType,
    pub my_nat_type: UdpNatType,
}

pub fn collect_udp_punch_tasks<I, F>(
    my_peer_id: PeerId,
    my_nat_type: UdpNatType,
    policy: P2pPolicyFlags,
    candidates: I,
    is_blacklisted: F,
) -> Vec<UdpPunchTaskInfo>
where
    I: IntoIterator<Item = UdpPunchCandidate>,
    F: Fn(PeerId) -> bool,
{
    if my_nat_type.is_open() {
        return Vec::new();
    }

    candidates
        .into_iter()
        .filter_map(|candidate| {
            // The strict policy still permits UDP punching when the resulting
            // session is upgraded to HTTP3. Both endpoints must select that
            // mode until the punch RPC negotiates a scheme per attempt.
            if policy.only_use_wss_http3_for_hole_punching
                != candidate.peer_disguise_flags.only_use_wss_http3_for_p2p
                || (policy.only_use_wss_http3_for_hole_punching
                    && !policy.use_wss_http3_with_peer(&candidate.peer_disguise_flags))
            {
                return None;
            }
            let static_allowed = should_background_p2p_with_peer(
                candidate.feature_flag.as_ref(),
                false,
                policy.lazy_p2p,
                policy.disable_p2p,
                policy.need_p2p,
            );
            let dynamic_allowed = should_try_p2p_with_peer(
                candidate.feature_flag.as_ref(),
                false,
                policy.disable_p2p,
                policy.need_p2p,
            ) && candidate.has_recent_traffic;
            if !static_allowed && !dynamic_allowed {
                return None;
            }

            let peer_id = candidate.peer_id;
            let already_connected =
                if policy.only_use_wss_http3_for_hole_punching && policy.prefer_http3_for_p2p {
                    candidate.has_http3_connection
                } else {
                    candidate.has_direct_connection
                };
            if peer_id == my_peer_id || is_blacklisted(peer_id) || already_connected {
                return None;
            }

            let peer_nat_type = candidate.udp_nat_type.into();
            if !my_nat_type.can_punch_hole_as_client(
                peer_nat_type,
                my_peer_id,
                peer_id,
                policy.disable_sym_hole_punching,
            ) {
                return None;
            }

            Some(UdpPunchTaskInfo {
                dst_peer_id: peer_id,
                dst_nat_type: peer_nat_type,
                my_nat_type,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::common::PeerFeatureFlag;

    fn candidate(peer_id: PeerId, udp_nat_type: NatType) -> UdpPunchCandidate {
        UdpPunchCandidate {
            peer_id,
            udp_nat_type,
            feature_flag: Some(PeerFeatureFlag::default()),
            has_direct_connection: false,
            has_http3_connection: false,
            has_recent_traffic: false,
            peer_disguise_flags: PeerDisguiseP2pFlags::default(),
        }
    }

    fn collect(
        my_peer_id: PeerId,
        my_nat_type: NatType,
        policy: P2pPolicyFlags,
        candidates: Vec<UdpPunchCandidate>,
    ) -> Vec<UdpPunchTaskInfo> {
        collect_udp_punch_tasks(my_peer_id, my_nat_type.into(), policy, candidates, |_| {
            false
        })
    }

    #[test]
    fn open_nat_does_not_start_udp_punch_tasks() {
        let tasks = collect(
            1,
            NatType::OpenInternet,
            P2pPolicyFlags::default(),
            vec![candidate(2, NatType::PortRestricted)],
        );

        assert!(tasks.is_empty());
    }

    #[test]
    fn wss_http3_restricted_mode_punches_compatible_peer() {
        let mut compatible = candidate(2, NatType::PortRestricted);
        compatible.peer_disguise_flags.only_use_wss_http3_for_p2p = true;
        let tasks = collect(
            1,
            NatType::PortRestricted,
            P2pPolicyFlags {
                only_use_wss_http3_for_hole_punching: true,
                ..Default::default()
            },
            vec![candidate(3, NatType::PortRestricted), compatible],
        );

        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].dst_peer_id, 2);
    }

    #[test]
    fn raw_udp_punch_skips_peer_requiring_http3() {
        let mut strict_peer = candidate(2, NatType::PortRestricted);
        strict_peer.peer_disguise_flags.only_use_wss_http3_for_p2p = true;
        let tasks = collect(
            1,
            NatType::PortRestricted,
            P2pPolicyFlags::default(),
            vec![strict_peer],
        );

        assert!(tasks.is_empty());
    }

    #[test]
    fn strict_http3_punch_continues_after_wss_connects() {
        let mut peer = candidate(2, NatType::PortRestricted);
        peer.peer_disguise_flags.only_use_wss_http3_for_p2p = true;
        peer.has_direct_connection = true;
        let policy = P2pPolicyFlags {
            only_use_wss_http3_for_hole_punching: true,
            prefer_http3_for_p2p: true,
            ..Default::default()
        };
        let mut self_candidate = peer.clone();
        self_candidate.peer_id = 1;
        assert_eq!(
            collect(
                1,
                NatType::PortRestricted,
                policy,
                vec![self_candidate, peer.clone()]
            )
            .len(),
            1
        );

        peer.has_http3_connection = true;
        assert!(collect(1, NatType::PortRestricted, policy, vec![peer]).is_empty());
    }

    #[test]
    fn strict_tcp_preference_keeps_existing_wss_connection() {
        let mut peer = candidate(2, NatType::PortRestricted);
        peer.peer_disguise_flags.only_use_wss_http3_for_p2p = true;
        peer.has_direct_connection = true;
        let policy = P2pPolicyFlags {
            only_use_wss_http3_for_hole_punching: true,
            prefer_http3_for_p2p: false,
            ..Default::default()
        };

        assert!(collect(1, NatType::PortRestricted, policy, vec![peer]).is_empty());
    }

    #[test]
    fn lazy_p2p_allows_recent_traffic_without_need_p2p_flag() {
        let mut idle = candidate(2, NatType::PortRestricted);
        idle.feature_flag = Some(PeerFeatureFlag {
            need_p2p: false,
            ..Default::default()
        });

        let mut active = idle.clone();
        active.peer_id = 3;
        active.has_recent_traffic = true;

        let tasks = collect(
            1,
            NatType::PortRestricted,
            P2pPolicyFlags {
                lazy_p2p: true,
                ..Default::default()
            },
            vec![idle, active],
        );

        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].dst_peer_id, 3);
    }

    #[test]
    fn skips_blacklisted_and_directly_connected_candidates() {
        let mut direct = candidate(2, NatType::PortRestricted);
        direct.has_direct_connection = true;

        let tasks = collect_udp_punch_tasks(
            1,
            NatType::PortRestricted.into(),
            P2pPolicyFlags::default(),
            vec![direct, candidate(3, NatType::PortRestricted)],
            |peer_id| peer_id == 3,
        );

        assert!(tasks.is_empty());
    }

    #[test]
    fn filters_candidates_by_udp_nat_method() {
        let tasks = collect(
            1,
            NatType::PortRestricted,
            P2pPolicyFlags::default(),
            vec![
                candidate(2, NatType::Symmetric),
                candidate(3, NatType::PortRestricted),
            ],
        );

        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].dst_peer_id, 3);
        assert_eq!(tasks[0].dst_nat_type, NatType::PortRestricted.into());
    }

    #[test]
    fn easy_symmetric_pair_uses_lower_peer_id_as_initiator() {
        let tasks = collect(
            1,
            NatType::SymmetricEasyInc,
            P2pPolicyFlags::default(),
            vec![candidate(2, NatType::SymmetricEasyDec)],
        );
        assert_eq!(tasks.len(), 1);

        let tasks = collect(
            2,
            NatType::SymmetricEasyInc,
            P2pPolicyFlags::default(),
            vec![candidate(1, NatType::SymmetricEasyDec)],
        );
        assert!(tasks.is_empty());
    }

    #[test]
    fn disabling_symmetric_hole_punch_keeps_sym_to_cone_as_cone_method() {
        let tasks = collect(
            1,
            NatType::Symmetric,
            P2pPolicyFlags {
                disable_sym_hole_punching: true,
                ..Default::default()
            },
            vec![candidate(2, NatType::PortRestricted)],
        );

        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].dst_peer_id, 2);
    }
}
