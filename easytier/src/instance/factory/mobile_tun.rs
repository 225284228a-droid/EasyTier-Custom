#[cfg(any(mobile, test))]
use std::collections::BTreeSet;

#[cfg(mobile)]
use super::{NativeInstanceManager, NativeInstanceRuntimeHost};
#[cfg(any(mobile, test))]
use crate::instance::shared_virtual_nic::{
    SharedIfConfig, SharedIfConfigClaims, SharedIfConfigConflict, SharedIpv4Route,
};
#[cfg(mobile)]
use easytier_core::instance::CoreInstanceState;

// Android exposes one capture fd for the process. Serialize the entire member
// transaction, including rollback, so separate instances cannot race to replace
// that fd with candidates from different generations.
#[cfg(mobile)]
static MOBILE_TUN_TRANSACTION: std::sync::LazyLock<tokio::sync::Mutex<MobileTunGenerationGate>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(MobileTunGenerationGate::default()));

#[cfg(any(mobile, test))]
#[derive(Default)]
struct MobileTunGenerationGate {
    latest: u64,
}

#[cfg(any(mobile, test))]
impl MobileTunGenerationGate {
    fn admit(&mut self, generation: Option<u64>) -> bool {
        match generation {
            Some(generation) if generation < self.latest => false,
            Some(generation) => {
                self.latest = generation;
                true
            }
            None => true,
        }
    }
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct MobileTunConfig {
    pub instance_ids: Vec<uuid::Uuid>,
    pub addresses: Vec<String>,
    pub routes: Vec<String>,
    pub dns: Option<String>,
    pub rejected: Vec<MobileTunMemberError>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct MobileTunMemberError {
    pub instance_id: uuid::Uuid,
    pub error: String,
}

#[cfg(any(mobile, test))]
struct MobileTunMember {
    instance_id: uuid::Uuid,
    claims: SharedIfConfigClaims,
    current_claims: Option<SharedIfConfigClaims>,
}

#[cfg(any(mobile, test))]
fn comparable_claims(mut claims: SharedIfConfigClaims) -> SharedIfConfigClaims {
    claims.mtu = None;
    claims
}

#[cfg(any(mobile, test))]
fn select_members(
    mut members: Vec<MobileTunMember>,
    mut rejected: Vec<MobileTunMemberError>,
) -> (Vec<MobileTunMember>, Vec<MobileTunMemberError>) {
    // Keep unchanged attached members ahead of new or reconfigured members.
    members.sort_by_key(|member| {
        !member.current_claims.as_ref().is_some_and(|current| {
            comparable_claims(current.clone()) == comparable_claims(member.claims.clone())
        })
    });
    let mut ifcfg = SharedIfConfig::default();
    let mut accepted = Vec::new();
    for member in members {
        let claims = member.claims.clone();
        if let Err(error) = ifcfg.ensure_disjoint(member.instance_id, &claims) {
            rejected.push(MobileTunMemberError {
                instance_id: member.instance_id,
                error: error.to_string(),
            });
            continue;
        }
        ifcfg.apply_member_claims(member.instance_id, claims);
        accepted.push(member);
    }
    (accepted, rejected)
}

#[cfg(any(mobile, test))]
fn config_for_members(
    members: &[MobileTunMember],
    rejected: Vec<MobileTunMemberError>,
) -> MobileTunConfig {
    let mut config = MobileTunConfig {
        rejected,
        ..Default::default()
    };
    let mut addresses = BTreeSet::new();
    let mut routes = BTreeSet::new();
    for member in members {
        config.instance_ids.push(member.instance_id);
        for address in &member.claims.ipv4_addresses {
            addresses.insert(format!("{address:#}"));
            routes.insert(format!("{:#}", address.network()));
        }
        for address in &member.claims.ipv6_addresses {
            addresses.insert(format!("{address:#}"));
            routes.insert(format!("{:#}", address.network()));
        }
        for route in &member.claims.ipv4_routes {
            if let Ok(route) = cidr::Ipv4Inet::new(route.address, route.prefix) {
                routes.insert(format!("{:#}", route.network()));
            }
        }
        for route in &member.claims.ipv6_routes {
            if let Ok(route) = cidr::Ipv6Inet::new(route.address, route.prefix) {
                routes.insert(format!("{:#}", route.network()));
            }
        }
    }
    config.instance_ids.sort();
    config.addresses = addresses.into_iter().collect();
    config.routes = routes.into_iter().collect();
    config
}

#[cfg(mobile)]
async fn collect_members(
    manager: &NativeInstanceManager,
    instance_ids: &[uuid::Uuid],
) -> (Vec<MobileTunMember>, Vec<MobileTunMemberError>) {
    let mut members = Vec::new();
    let mut rejected = Vec::new();
    let mut seen = BTreeSet::new();
    for &instance_id in instance_ids {
        if !seen.insert(instance_id) {
            continue;
        }
        let instance = manager.instance(instance_id);
        let runtime = instance
            .as_ref()
            .filter(|instance| instance.state() == CoreInstanceState::Running)
            .and_then(|instance| instance.runtime_host::<NativeInstanceRuntimeHost>())
            .filter(|runtime| runtime.tun_enabled());
        let Some(runtime) = runtime else {
            rejected.push(MobileTunMemberError {
                instance_id,
                error: "TUN-enabled instance is not running".to_owned(),
            });
            continue;
        };
        let (claims, current_claims) = runtime
            .mobile_tun_claims(&instance.as_ref().unwrap().packet_plane())
            .await;
        members.push(MobileTunMember {
            instance_id,
            claims,
            current_claims,
        });
    }
    (members, rejected)
}

#[cfg(mobile)]
pub async fn get_mobile_tun_config(
    manager: &NativeInstanceManager,
    instance_ids: &[uuid::Uuid],
) -> MobileTunConfig {
    let (members, rejected) = collect_members(manager, instance_ids).await;
    let (members, rejected) = select_members(members, rejected);
    config_for_members(&members, rejected)
}

#[cfg(any(mobile, test))]
fn is_claim_conflict(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|error| error.is::<SharedIfConfigConflict>())
}

#[cfg(mobile)]
pub async fn attach_mobile_tun_fd(
    manager: &NativeInstanceManager,
    fd: i32,
    instance_ids: Option<&[uuid::Uuid]>,
) -> anyhow::Result<MobileTunConfig> {
    attach_mobile_tun_fd_with_generation(manager, fd, instance_ids, None).await
}

#[cfg(mobile)]
pub async fn attach_mobile_tun_fd_with_generation(
    manager: &NativeInstanceManager,
    fd: i32,
    instance_ids: Option<&[uuid::Uuid]>,
    generation: Option<u64>,
) -> anyhow::Result<MobileTunConfig> {
    let mut transaction = MOBILE_TUN_TRANSACTION.lock().await;
    if !transaction.admit(generation) {
        return Err(super::super::runtime_host::StaleMobileTunGeneration(generation).into());
    }
    let all_ids = manager
        .instances()
        .into_iter()
        .filter(|instance| {
            instance.state() == CoreInstanceState::Running
                && instance
                    .runtime_host::<NativeInstanceRuntimeHost>()
                    .is_some_and(NativeInstanceRuntimeHost::tun_enabled)
        })
        .map(|instance| instance.instance_id())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if fd <= 0 {
        let mut errors = Vec::new();
        for id in all_ids {
            if let Err(error) = attach_member(manager, id, 0, false, None, generation).await {
                errors.push(error.to_string());
            }
        }
        anyhow::ensure!(
            errors.is_empty(),
            "failed to detach mobile TUN fd: {}",
            errors.join("; ")
        );
        return Ok(MobileTunConfig::default());
    }
    let (members, rejected) = collect_members(manager, instance_ids.unwrap_or(&all_ids)).await;
    let (members, mut rejected) = select_members(members, rejected);
    let selected = members
        .iter()
        .map(|member| member.instance_id)
        .collect::<BTreeSet<_>>();
    for id in all_ids.iter().filter(|id| !selected.contains(id)) {
        // The OS candidate contains only selected claims. Remove prior members
        // omitted from that candidate before changing the shared capture fd.
        attach_member(manager, *id, 0, false, None, generation).await?;
    }
    let mut attached = Vec::new();
    // Open once; a rejected member never replaces or tears down this shared fd.
    for member in members {
        match attach_member(
            manager,
            member.instance_id,
            fd,
            attached.is_empty(),
            Some(member.claims.clone()),
            generation,
        )
        .await
        {
            Ok(()) => attached.push(member),
            Err(error) if is_claim_conflict(&error) => {
                attach_member(manager, member.instance_id, 0, false, None, generation).await?;
                rejected.push(MobileTunMemberError {
                    instance_id: member.instance_id,
                    error: error.to_string(),
                });
            }
            Err(error)
                if error.chain().any(|error| {
                    error.is::<super::super::runtime_host::StaleMobileTunGeneration>()
                }) =>
            {
                // A newer candidate owns the shared TUN now. Rolling back this
                // superseded transaction must never detach that newer member.
                return Err(error);
            }
            Err(error) => {
                for id in &all_ids {
                    let _ = attach_member(manager, *id, 0, false, None, generation).await;
                }
                return Err(error.context("failed to attach mobile TUN fd"));
            }
        }
    }
    Ok(config_for_members(&attached, rejected))
}

#[cfg(mobile)]
async fn attach_member(
    manager: &NativeInstanceManager,
    instance_id: uuid::Uuid,
    fd: i32,
    replace_tun_fd: bool,
    claims: Option<SharedIfConfigClaims>,
    generation: Option<u64>,
) -> anyhow::Result<()> {
    let instance = manager
        .instance(instance_id)
        .ok_or_else(|| anyhow::anyhow!("instance {instance_id} is unavailable"))?;
    let runtime = instance
        .runtime_host::<NativeInstanceRuntimeHost>()
        .ok_or_else(|| anyhow::anyhow!("native runtime host is unavailable"))?;
    runtime
        .attach_mobile_tun_fd_with_generation(fd, replace_tun_fd, claims, generation)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instance::shared_virtual_nic::SharedIpv6Route;

    #[test]
    fn superseded_transaction_cannot_attach_or_roll_back_a_newer_capture() {
        let mut gate = MobileTunGenerationGate::default();
        assert!(gate.admit(Some(19)));
        assert!(gate.admit(Some(20)));
        // Both attachment and failure rollback pass through the same admission.
        assert!(!gate.admit(Some(19)));
        assert!(gate.admit(Some(20)), "the current candidate can retry");
        assert!(gate.admit(None), "legacy callers remain compatible");
        assert!(!gate.admit(Some(19)));
    }

    fn member(id: u128, address: &str) -> MobileTunMember {
        MobileTunMember {
            instance_id: uuid::Uuid::from_u128(id),
            claims: SharedIfConfigClaims {
                ipv4_addresses: BTreeSet::from([address.parse().unwrap()]),
                ..Default::default()
            },
            current_claims: None,
        }
    }

    #[test]
    fn keeps_attached_member_when_bootstrap_subnets_overlap() {
        let mut existing = member(2, "10.126.126.2/24");
        existing.current_claims = Some(existing.claims.clone());
        let (members, rejected) =
            select_members(vec![member(1, "10.126.126.1/24"), existing], Vec::new());
        let config = config_for_members(&members, rejected);
        assert_eq!(config.instance_ids, vec![uuid::Uuid::from_u128(2)]);
        assert_eq!(config.addresses, vec!["10.126.126.2/24"]);
        assert_eq!(config.routes, vec!["10.126.126.0/24"]);
        assert_eq!(config.rejected[0].instance_id, uuid::Uuid::from_u128(1));
    }

    #[test]
    fn reconfigured_member_does_not_displace_unchanged_member() {
        let mut changed = member(1, "10.0.2.1/24");
        changed.current_claims = Some(member(1, "10.0.1.1/24").claims);
        let mut unchanged = member(2, "10.0.2.2/24");
        unchanged.current_claims = Some(unchanged.claims.clone());
        unchanged.current_claims.as_mut().unwrap().mtu = Some(1300);
        let (members, rejected) = select_members(vec![changed, unchanged], Vec::new());
        let config = config_for_members(&members, rejected);
        assert_eq!(config.instance_ids, vec![uuid::Uuid::from_u128(2)]);
        assert_eq!(config.rejected[0].instance_id, uuid::Uuid::from_u128(1));
    }

    #[test]
    fn projects_ipv6_and_rejects_overlapping_proxy_routes() {
        let mut first = member(1, "10.0.1.1/24");
        first
            .claims
            .ipv4_addresses
            .insert("10.0.4.1/32".parse().unwrap());
        first
            .claims
            .ipv6_addresses
            .insert("2001:db8::1/128".parse().unwrap());
        first
            .claims
            .ipv6_routes
            .insert(SharedIpv6Route::new("::".parse().unwrap(), 0, Some(5)));
        let second = member(2, "10.0.2.1/24");
        let (members, rejected) = select_members(
            vec![first, second, member(3, "100.100.100.101/32")],
            Vec::new(),
        );
        let config = config_for_members(&members, rejected);
        assert_eq!(config.instance_ids.len(), 3);
        assert!(config.addresses.contains(&"10.0.4.1/32".to_owned()));
        assert!(config.routes.contains(&"10.0.4.1/32".to_owned()));
        assert!(config.addresses.contains(&"2001:db8::1/128".to_owned()));
        assert!(config.routes.contains(&"2001:db8::1/128".to_owned()));
        assert!(config.routes.contains(&"::/0".to_owned()));
        assert_eq!(config.dns, None);
        assert!(config.rejected.is_empty());
        let mut route_owner = member(2, "10.0.2.1/24");
        route_owner.claims.ipv4_routes.insert(SharedIpv4Route::new(
            "10.0.1.0".parse().unwrap(),
            24,
            None,
        ));
        let (members, rejected) =
            select_members(vec![member(1, "10.0.1.1/24"), route_owner], Vec::new());
        let config = config_for_members(&members, rejected);
        assert_eq!(config.instance_ids.len(), 1);
        assert_eq!(config.rejected[0].instance_id, uuid::Uuid::from_u128(2));
    }

    #[test]
    fn distinguishes_claim_conflicts_from_device_failures_through_context() {
        let mut claims = SharedIfConfig::default();
        claims.apply_member_claims(uuid::Uuid::from_u128(1), member(1, "10.0.1.1/24").claims);
        let conflict = claims
            .ensure_disjoint(uuid::Uuid::from_u128(2), &member(2, "10.0.1.2/24").claims)
            .unwrap_err();
        let error = anyhow::Error::new(crate::common::error::Error::from(conflict))
            .context("attach mobile TUN failed");
        assert!(is_claim_conflict(&error));
        assert!(!is_claim_conflict(&anyhow::Error::new(
            std::io::Error::from_raw_os_error(19)
        )));
    }
}
