use std::sync::{Arc, OnceLock};

use easytier_core::host::packet::HostPacketReceiver;
use tokio::{sync::Mutex, task::JoinSet};

use super::MagicDnsRuntime;
use crate::instance::virtual_nic::NicCtx;

struct NicCtxContainer {
    nic_ctx: NicOwner,
    magic_dns: MagicDnsRuntime,
}

enum NicOwner {
    Interface(NicCtx),
    Drain(JoinSet<()>),
}

impl NicCtxContainer {
    fn new(nic_ctx: NicCtx, magic_dns: MagicDnsRuntime) -> Self {
        Self {
            nic_ctx: NicOwner::Interface(nic_ctx),
            magic_dns,
        }
    }

    fn packet_drain(tasks: JoinSet<()>) -> Self {
        Self {
            nic_ctx: NicOwner::Drain(tasks),
            magic_dns: MagicDnsRuntime::default(),
        }
    }

    async fn shutdown(&mut self) {
        self.magic_dns.stop().await;
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
        if status == TunGenerationStatus::Install {
            if let Some(generation) = generation {
                self.committed = generation;
            }
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

    pub(super) async fn drain(&self) {
        self.stop().await;
        let receiver = self.receiver();
        let mut tasks = JoinSet::new();
        tasks.spawn(async move {
            let mut receiver = receiver.lock().await;
            while let Some(packet) = receiver.recv().await {
                tracing::trace!(?packet, "discarded packet without a native interface");
            }
        });
        self.nic_ctx.lock().await.active = Some(NicCtxContainer::packet_drain(tasks));
    }

    pub(super) async fn announce_generation(
        &self,
        generation: Option<u64>,
    ) -> anyhow::Result<bool> {
        let status = self.generations.lock().await.announce(generation);
        match status {
            TunGenerationStatus::Install => Ok(true),
            TunGenerationStatus::AlreadyCommitted => Ok(false),
            TunGenerationStatus::Stale => {
                anyhow::bail!("stale mobile TUN generation {generation:?}")
            }
        }
    }

    pub(super) async fn generation_status(&self, generation: Option<u64>) -> TunGenerationStatus {
        self.generations.lock().await.status(generation)
    }

    pub(super) async fn install<F>(
        &self,
        generation: Option<u64>,
        nic: NicCtx,
        make_magic_dns: F,
    ) -> anyhow::Result<()>
    where
        F: FnOnce() -> MagicDnsRuntime,
    {
        let mut state = self.nic_ctx.lock().await;
        let mut generations = self.generations.lock().await;
        let status = generations.status(generation);
        if status != TunGenerationStatus::Install {
            drop(generations);
            drop(state);
            let mut stale = NicCtxContainer::new(nic, MagicDnsRuntime::default());
            stale.shutdown().await;
            return match status {
                TunGenerationStatus::AlreadyCommitted => Ok(()),
                TunGenerationStatus::Stale => {
                    anyhow::bail!("stale mobile TUN generation {generation:?}")
                }
                TunGenerationStatus::Install => unreachable!(),
            };
        }
        if let Some(current) = state.active.as_mut() {
            let mut old_dns = std::mem::take(&mut current.magic_dns);
            old_dns.stop().await;
        }
        let status = generations.commit(generation);
        debug_assert_eq!(status, TunGenerationStatus::Install);
        let replacement = NicCtxContainer::new(nic, make_magic_dns());
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
    use super::{TunGenerationGate, TunGenerationStatus};

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
    fn legacy_untagged_attach_keeps_working_without_advancing_high_water() {
        let mut gate = TunGenerationGate::default();

        assert_eq!(gate.announce(None), TunGenerationStatus::Install);
        assert_eq!(gate.announce(Some(1)), TunGenerationStatus::Install);
        assert_eq!(gate.commit(Some(1)), TunGenerationStatus::Install);
        assert_eq!(gate.announce(None), TunGenerationStatus::Install);
        assert_eq!(gate.committed, 1);
    }
}
