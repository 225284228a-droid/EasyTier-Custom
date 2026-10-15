use std::sync::{Arc, OnceLock};

use crate::common::global_ctx::ArcGlobalCtx;
#[cfg(not(mobile))]
use crate::instance::shared_virtual_nic::ArcSharedVirtualNicRegistry;
#[cfg(not(mobile))]
use easytier_core::instance::CorePacketPlane;
use easytier_core::{gateway::dhcp::DhcpAddresses, host::packet::HostPacketReceiver};
#[cfg(not(mobile))]
use futures::FutureExt as _;
#[cfg(not(mobile))]
use tokio::sync::Notify;
use tokio::{sync::Mutex, task::JoinSet};
use tokio_util::sync::CancellationToken;

use crate::instance::virtual_nic::NicCtx;

struct NicCtxContainer {
    nic_ctx: NicOwner,
}

enum NicOwner {
    Interface(NicCtx),
    Drain(JoinSet<()>),
}

impl NicCtxContainer {
    fn new(nic_ctx: NicCtx) -> Self {
        Self {
            nic_ctx: NicOwner::Interface(nic_ctx),
        }
    }

    fn packet_drain(tasks: JoinSet<()>) -> Self {
        Self {
            nic_ctx: NicOwner::Drain(tasks),
        }
    }

    async fn shutdown(&mut self) {
        match &mut self.nic_ctx {
            NicOwner::Interface(nic) => nic.shutdown().await,
            NicOwner::Drain(tasks) => tasks.shutdown().await,
        }
    }
}

#[derive(Clone)]
pub(super) struct TunNicState {
    nic_ctx: Arc<Mutex<TunNicStateInner>>,
    generations: Arc<Mutex<TunGenerationGate>>,
    receiver: Arc<OnceLock<Arc<Mutex<HostPacketReceiver>>>>,
}

#[derive(Default)]
struct TunNicStateInner {
    active: Option<NicCtxContainer>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum TunGenerationStatus {
    Install,
    AlreadyCommitted,
    Stale,
}

#[derive(Default)]
struct TunGenerationGate {
    latest_requested: u64,
    committed: u64,
}

impl TunGenerationGate {
    #[cfg(any(mobile, test))]
    fn announce(&mut self, generation: Option<u64>) -> TunGenerationStatus {
        match generation {
            None => TunGenerationStatus::Install,
            Some(generation) if generation < self.latest_requested => TunGenerationStatus::Stale,
            Some(generation) if generation == self.committed && generation > 0 => {
                TunGenerationStatus::AlreadyCommitted
            }
            Some(generation) if generation < self.committed => TunGenerationStatus::Stale,
            Some(generation) => {
                self.latest_requested = self.latest_requested.max(generation);
                TunGenerationStatus::Install
            }
        }
    }

    fn status(&self, generation: Option<u64>) -> TunGenerationStatus {
        match generation {
            None => TunGenerationStatus::Install,
            Some(generation) if generation < self.latest_requested => TunGenerationStatus::Stale,
            Some(generation) if generation == self.committed && generation > 0 => {
                TunGenerationStatus::AlreadyCommitted
            }
            Some(generation)
                if generation == self.latest_requested && generation > self.committed =>
            {
                TunGenerationStatus::Install
            }
            Some(_) => TunGenerationStatus::Stale,
        }
    }

    fn commit(&mut self, generation: Option<u64>) -> TunGenerationStatus {
        let status = self.status(generation);
        if status == TunGenerationStatus::Install
            && let Some(generation) = generation
        {
            self.committed = generation;
        }
        status
    }

    #[cfg(any(mobile, test))]
    fn detach(&mut self, generation: Option<u64>) -> TunGenerationStatus {
        let status = self.status(generation);
        if status != TunGenerationStatus::Stale {
            // A rollback may retry the same generation after replacing its fd.
            // Retain the high-water mark while clearing only attachment state.
            self.committed = 0;
        }
        status
    }
}

impl TunNicState {
    pub(super) fn empty() -> Self {
        Self {
            nic_ctx: Arc::new(Mutex::new(TunNicStateInner::default())),
            generations: Arc::new(Mutex::new(TunGenerationGate::default())),
            receiver: Arc::new(OnceLock::new()),
        }
    }

    pub(super) fn install_receiver(&self, receiver: HostPacketReceiver) -> anyhow::Result<()> {
        self.receiver
            .set(Arc::new(Mutex::new(receiver)))
            .map_err(|_| anyhow::anyhow!("native packet receiver is already installed"))
    }

    pub(super) fn receiver(&self) -> Arc<Mutex<HostPacketReceiver>> {
        self.receiver
            .get()
            .expect("packet receiver must be installed before preparing TUN")
            .clone()
    }

    pub(super) async fn stop(&self) {
        let mut old = self.nic_ctx.lock().await.active.take();
        if let Some(nic) = old.as_mut() {
            nic.shutdown().await;
        }
        drop(old);
    }

    #[cfg(mobile)]
    pub(super) async fn mobile_tun_claims(
        &self,
    ) -> Option<crate::instance::shared_virtual_nic::SharedIfConfigClaims> {
        let state = self.nic_ctx.lock().await;
        match &state.active.as_ref()?.nic_ctx {
            NicOwner::Interface(nic) => nic.current_mobile_tun_claims().await,
            NicOwner::Drain(_) => None,
        }
    }

    pub(super) async fn drain(&self) {
        self.stop().await;
        self.nic_ctx.lock().await.active = Some(self.new_drain());
    }

    fn new_drain(&self) -> NicCtxContainer {
        let receiver = self.receiver();
        let mut tasks = JoinSet::new();
        tasks.spawn(async move {
            let mut receiver = receiver.lock().await;
            while let Some(packet) = receiver.recv().await {
                tracing::trace!(?packet, "discarded packet without a native interface");
            }
        });
        NicCtxContainer::packet_drain(tasks)
    }

    #[cfg(any(mobile, test))]
    pub(super) async fn drain_for_generation(&self, generation: Option<u64>) -> anyhow::Result<()> {
        let mut state = self.nic_ctx.lock().await;
        let mut generations = self.generations.lock().await;
        if generations.detach(generation) == TunGenerationStatus::Stale {
            return Err(super::StaleMobileTunGeneration(generation).into());
        }
        // Check and remove atomically with installation. A superseded rollback
        // can never take a NIC installed by a newer candidate.
        let mut old = state.active.replace(self.new_drain());
        drop(generations);
        drop(state);
        if let Some(nic) = old.as_mut() {
            nic.shutdown().await;
        }
        Ok(())
    }

    #[cfg(mobile)]
    pub(super) async fn announce_generation(
        &self,
        generation: Option<u64>,
    ) -> anyhow::Result<bool> {
        let status = self.generations.lock().await.announce(generation);
        match status {
            TunGenerationStatus::Install => Ok(true),
            TunGenerationStatus::AlreadyCommitted => Ok(false),
            TunGenerationStatus::Stale => {
                return Err(super::StaleMobileTunGeneration(generation).into());
            }
        }
    }

    #[cfg(mobile)]
    pub(super) async fn generation_status(&self, generation: Option<u64>) -> TunGenerationStatus {
        self.generations.lock().await.status(generation)
    }

    pub(super) async fn install(&self, generation: Option<u64>, nic: NicCtx) -> anyhow::Result<()> {
        let mut state = self.nic_ctx.lock().await;
        let mut generations = self.generations.lock().await;
        let status = generations.status(generation);
        if status != TunGenerationStatus::Install {
            drop(generations);
            drop(state);
            let mut stale = NicCtxContainer::new(nic);
            stale.shutdown().await;
            return match status {
                TunGenerationStatus::AlreadyCommitted => Ok(()),
                TunGenerationStatus::Stale => {
                    return Err(super::StaleMobileTunGeneration(generation).into());
                }
                TunGenerationStatus::Install => unreachable!(),
            };
        }
        let status = generations.commit(generation);
        debug_assert_eq!(status, TunGenerationStatus::Install);
        let replacement = NicCtxContainer::new(nic);
        let mut old = state.active.replace(replacement);
        drop(generations);
        drop(state);
        if let Some(nic) = old.as_mut() {
            nic.shutdown().await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{NicCtxContainer, TunGenerationGate, TunGenerationStatus, TunNicState};

    #[tokio::test]
    async fn stale_detach_preserves_the_newer_nic_owner() {
        let state = TunNicState::empty();
        let (held, mut released) = tokio::sync::oneshot::channel::<()>();
        let mut tasks = tokio::task::JoinSet::new();
        tasks.spawn(async move {
            let _held = held;
            std::future::pending::<()>().await;
        });
        state.nic_ctx.lock().await.active = Some(NicCtxContainer::packet_drain(tasks));
        {
            let mut gate = state.generations.lock().await;
            assert_eq!(gate.announce(Some(20)), TunGenerationStatus::Install);
            assert_eq!(gate.commit(Some(20)), TunGenerationStatus::Install);
        }

        let error = state.drain_for_generation(Some(19)).await.unwrap_err();
        assert!(error.is::<super::super::StaleMobileTunGeneration>());
        assert_eq!(
            released.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty),
            "a stale rollback must retain the current owner's resources"
        );
        state.stop().await;
        assert!(released.await.is_err());
    }

    #[test]
    fn delayed_older_completion_is_rejected_after_newer_commit() {
        let mut gate = TunGenerationGate::default();

        assert_eq!(gate.announce(Some(19)), TunGenerationStatus::Install);
        assert_eq!(gate.announce(Some(20)), TunGenerationStatus::Install);
        assert_eq!(gate.commit(Some(19)), TunGenerationStatus::Stale);
        assert_eq!(gate.commit(Some(20)), TunGenerationStatus::Install);
        assert_eq!(gate.announce(Some(19)), TunGenerationStatus::Stale);
        assert_eq!(gate.committed, 20);
    }

    #[test]
    fn failed_candidate_does_not_advance_generation_until_commit() {
        let mut gate = TunGenerationGate::default();

        assert_eq!(gate.announce(Some(7)), TunGenerationStatus::Install);
        assert_eq!(gate.status(Some(7)), TunGenerationStatus::Install);
        assert_eq!(gate.announce(Some(7)), TunGenerationStatus::Install);
        assert_eq!(gate.commit(Some(7)), TunGenerationStatus::Install);
        assert_eq!(gate.committed, 7);
    }

    #[test]
    fn detached_candidate_can_retry_without_admitting_older_generations() {
        let mut gate = TunGenerationGate::default();
        assert_eq!(gate.announce(Some(20)), TunGenerationStatus::Install);
        assert_eq!(gate.commit(Some(20)), TunGenerationStatus::Install);
        assert_eq!(gate.detach(Some(19)), TunGenerationStatus::Stale);
        assert_eq!(gate.status(Some(20)), TunGenerationStatus::AlreadyCommitted);
        assert_eq!(gate.detach(Some(20)), TunGenerationStatus::AlreadyCommitted);
        assert_eq!(gate.announce(Some(20)), TunGenerationStatus::Install);
        assert_eq!(gate.commit(Some(20)), TunGenerationStatus::Install);
        assert_eq!(gate.announce(Some(19)), TunGenerationStatus::Stale);
    }

    #[test]
    fn legacy_untagged_attach_keeps_working_without_advancing_high_water() {
        let mut gate = TunGenerationGate::default();

        assert_eq!(gate.announce(None), TunGenerationStatus::Install);
        assert_eq!(gate.announce(Some(1)), TunGenerationStatus::Install);
        assert_eq!(gate.commit(Some(1)), TunGenerationStatus::Install);
        assert_eq!(gate.announce(None), TunGenerationStatus::Install);
        assert_eq!(gate.committed, 1);
    }
}

pub(super) struct NativeDhcpHost {
    pub(super) global_ctx: ArcGlobalCtx,
    pub(super) operation: Arc<Mutex<()>>,
    pub(super) cancel: CancellationToken,
    pub(super) nic: TunNicState,
    #[cfg(not(mobile))]
    pub(super) closed: Arc<Notify>,
    #[cfg(not(mobile))]
    pub(super) packet_plane: Arc<CorePacketPlane>,
    #[cfg(not(mobile))]
    pub(super) shared_virtual_nic_registry: ArcSharedVirtualNicRegistry,
}

impl NativeDhcpHost {
    pub(super) fn take_closed(&self) -> bool {
        #[cfg(not(mobile))]
        {
            self.closed.notified().now_or_never().is_some()
        }
        #[cfg(mobile)]
        {
            // The mobile VPN owner recreates and attaches its interface in response
            // to address events; desktop interface notifications do not apply here.
            false
        }
    }

    pub(super) async fn apply(&self, next: DhcpAddresses) -> anyhow::Result<()> {
        self.ensure_open()?;
        // The static family may already have an attached VPN interface before
        // DHCP's first round. No address event will rebuild it if nothing changed.
        #[cfg(mobile)]
        if next == self.current() {
            return Ok(());
        }
        tokio::select! {
            _ = self.cancel.cancelled() => anyhow::bail!("instance is closing; DHCP update cancelled"),
            _ = self.nic.drain() => {}
        }
        #[cfg(not(mobile))]
        self.set_addresses(DhcpAddresses::default());
        self.ensure_open()?;

        #[cfg(not(mobile))]
        {
            let addresses = self.with_static_addresses(next);
            if !self.global_ctx.no_tun() && (addresses.ipv4.is_some() || addresses.ipv6.is_some()) {
                let mut nic = NicCtx::new(
                    self.global_ctx.clone(),
                    self.packet_plane.clone(),
                    self.nic.receiver(),
                    self.closed.clone(),
                    self.shared_virtual_nic_registry.clone(),
                )
                .await;
                tokio::select! {
                    _ = self.cancel.cancelled() => {
                        nic.shutdown().await;
                        anyhow::bail!("instance is closing; DHCP update cancelled");
                    },
                    result = nic.run(addresses.ipv4, addresses.ipv6) => result?,
                }
                self.nic.install(None, nic).await?;
            }
        }
        self.set_addresses(next);
        Ok(())
    }
}

#[cfg(test)]
mod dhcp_tests {
    use easytier_core::{config::toml::TomlConfig, process_runtime::CoreProcessRuntime};

    use super::*;
    use crate::{common::global_ctx::GlobalCtxEvent, instance::test_instance::TestInstance};

    #[tokio::test]
    async fn stop_releases_packet_receiver_before_returning() {
        let state = TunNicState::empty();
        let (_sender, receiver) = tokio::sync::mpsc::channel(1);
        state
            .install_receiver(HostPacketReceiver::new(receiver))
            .unwrap();
        let receiver = state.receiver();
        state.drain().await;
        // Let the drain acquire the receiver and block waiting for a packet.
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while receiver.try_lock().is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        state.stop().await;
        assert!(receiver.try_lock().is_ok());
    }

    #[tokio::test]
    async fn dhcp_preserves_publication_permit_and_cancelled_address() {
        let instance = TestInstance::new_with_process_runtime(
            TomlConfig::new_from_str("dhcp = true\ndhcpv6 = true").unwrap(),
            CoreProcessRuntime::new(),
        );
        let global_ctx = instance.get_global_ctx();
        let mut flags = global_ctx.get_flags();
        flags.no_tun = true;
        global_ctx.set_flags(flags);
        let operation = Arc::new(Mutex::new(()));
        let cancel = CancellationToken::new();
        let runtime = super::super::NativeTunRuntime::new(
            global_ctx.clone(),
            cancel.clone(),
            TestInstance::new_shared_virtual_nic_registry(),
        );
        let (_sender, receiver) = tokio::sync::mpsc::channel(1);
        runtime
            .install_packet_receiver(HostPacketReceiver::new(receiver))
            .unwrap();
        let host = runtime.dhcp_host(
            operation.clone(),
            instance.get_core_instance().packet_plane(),
        );
        let mut events = global_ctx.subscribe();
        let addresses = DhcpAddresses {
            ipv4: Some("10.20.0.7/24".parse().unwrap()),
            ipv6: Some("fd12::7/64".parse().unwrap()),
        };
        let mut previous = DhcpAddresses::default();
        for next in [addresses, DhcpAddresses::default()] {
            let outcome = host.apply_dhcp(next).await;
            assert!(outcome.result.is_ok());
            assert_eq!(outcome.actual, next);
            assert_eq!(global_ctx.get_ipv4(), next.ipv4);
            assert_eq!(global_ctx.get_ipv6(), next.ipv6);
            assert!(operation.try_lock().is_err());
            host.publish_dhcp(previous, outcome.actual);
            assert_eq!(
                events.try_recv().unwrap(),
                if next.ipv4.is_none() {
                    GlobalCtxEvent::DhcpIpv4Conflicted(previous.ipv4)
                } else {
                    GlobalCtxEvent::DhcpIpv4Changed(previous.ipv4, next.ipv4)
                }
            );
            assert_eq!(
                events.try_recv().unwrap(),
                if next.ipv6.is_none() {
                    GlobalCtxEvent::DhcpIpv6Conflicted(previous.ipv6)
                } else {
                    GlobalCtxEvent::DhcpIpv6Changed(previous.ipv6, next.ipv6)
                }
            );
            drop(outcome);
            assert!(operation.try_lock().is_ok());
            previous = next;
        }

        global_ctx.set_ipv4(addresses.ipv4);
        global_ctx.set_ipv6(addresses.ipv6);
        cancel.cancel();
        let outcome = host.apply_dhcp(DhcpAddresses::default()).await;
        assert!(outcome.result.is_err());
        assert_eq!(outcome.actual, addresses);
        assert_eq!(global_ctx.get_ipv4(), addresses.ipv4);
        assert_eq!(global_ctx.get_ipv6(), addresses.ipv6);
        runtime.shutdown().await;
    }
}
