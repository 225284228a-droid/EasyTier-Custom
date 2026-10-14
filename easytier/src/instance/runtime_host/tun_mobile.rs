use std::sync::Arc;

use anyhow::Context as _;
use cidr::Ipv4Inet;
use easytier_core::{
    gateway::dhcp::{DhcpIpv4ApplyOutcome, DhcpIpv4ApplyPermit, DhcpIpv4Host},
    host::packet::HostPacketReceiver,
    instance::CorePacketPlane,
};
use futures::FutureExt as _;
use tokio::sync::{Mutex, Notify, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::tun_common::{TunGenerationStatus, TunNicState};
use crate::{
    common::global_ctx::{ArcGlobalCtx, GlobalCtxEvent},
    instance::virtual_nic::NicCtx,
};

pub(super) struct NativeTunRuntime {
    global_ctx: ArcGlobalCtx,
    cancel: CancellationToken,
    nic: TunNicState,
    tun_fd: mpsc::Sender<TunFdRequest>,
    tun_fd_receiver: Mutex<Option<mpsc::Receiver<TunFdRequest>>>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

struct TunFdRequest {
    fd: i32,
    generation: Option<u64>,
    completion: Option<oneshot::Sender<Result<(), String>>>,
}

impl NativeTunRuntime {
    pub(super) fn new(global_ctx: ArcGlobalCtx, cancel: CancellationToken) -> Self {
        let (tun_fd, tun_fd_receiver) = mpsc::channel(16);
        Self {
            global_ctx,
            cancel,
            nic: TunNicState::empty(),
            tun_fd,
            tun_fd_receiver: Mutex::new(Some(tun_fd_receiver)),
            task: Mutex::new(None),
        }
    }

    pub(super) fn install_packet_receiver(
        &self,
        receiver: HostPacketReceiver,
    ) -> anyhow::Result<()> {
        self.nic.install_receiver(receiver)
    }

    async fn install_mobile_tun(
        nic_state: TunNicState,
        global_ctx: ArcGlobalCtx,
        packet_plane: Arc<CorePacketPlane>,
        fd: i32,
        generation: Option<u64>,
    ) -> anyhow::Result<()> {
        if fd <= 0 {
            anyhow::bail!("invalid mobile TUN fd: {fd}");
        }
        let closed = Arc::new(Notify::new());
        let mut nic = NicCtx::new(
            global_ctx.clone(),
            packet_plane.clone(),
            nic_state.receiver(),
            closed,
        );
        nic.run_for_mobile(fd).await.context("add ip failed")?;
        nic_state.install(generation, nic).await
    }

    pub(super) async fn prepare(&self, packet_plane: Arc<CorePacketPlane>) -> anyhow::Result<()> {
        self.nic.drain().await;
        let Some(mut tun_fds) = self.tun_fd_receiver.lock().await.take() else {
            return Ok(());
        };
        let nic_state = self.nic.clone();
        let global_ctx = self.global_ctx.clone();
        let cancel = self.cancel.clone();
        self.task.lock().await.replace(tokio::spawn(async move {
            loop {
                let request = tokio::select! {
                    _ = cancel.cancelled() => return,
                    request = tun_fds.recv() => match request { Some(request) => request, None => return },
                };
                let result = match nic_state.generation_status(request.generation).await {
                    TunGenerationStatus::AlreadyCommitted => Ok(()),
                    TunGenerationStatus::Stale => Err(anyhow::anyhow!(
                        "stale mobile TUN generation {:?}",
                        request.generation
                    )),
                    TunGenerationStatus::Install => Self::install_mobile_tun(
                        nic_state.clone(),
                        global_ctx.clone(),
                        packet_plane.clone(),
                        request.fd,
                        request.generation,
                    )
                    .await,
                };
                if let Err(error) = &result {
                    tracing::error!(?error, fd = request.fd, "failed to attach mobile TUN fd");
                }
                if let Some(completion) = request.completion {
                    let result = result.map_err(|error| format!("{error:#}"));
                    let _ = completion.send(result);
                }
            }
        }));
        Ok(())
    }

    pub(super) async fn shutdown(&self) {
        if let Some(task) = self.task.lock().await.take() {
            let _ = task.await;
        }
        self.nic.stop().await;
    }

    pub(super) fn attach_fd(&self, fd: i32) -> anyhow::Result<()> {
        self.tun_fd
            .try_send(TunFdRequest {
                fd,
                generation: None,
                completion: None,
            })
            .map_err(|error| anyhow::anyhow!("failed to send TUN fd: {error}"))
    }

    pub(super) async fn attach_fd_and_wait(
        &self,
        fd: i32,
        generation: Option<u64>,
    ) -> anyhow::Result<()> {
        if !self.nic.announce_generation(generation).await? {
            return Ok(());
        }
        let (completion, completed) = oneshot::channel();
        self.tun_fd
            .send(TunFdRequest {
                fd,
                generation,
                completion: Some(completion),
            })
            .await
            .map_err(|error| anyhow::anyhow!("failed to send TUN fd: {error}"))?;
        completed
            .await
            .context("mobile TUN installation acknowledgement was dropped")?
            .map_err(anyhow::Error::msg)
    }

    pub(super) fn dhcp_host(
        &self,
        operation: Arc<Mutex<()>>,
        _packet_plane: Arc<CorePacketPlane>,
    ) -> Arc<dyn DhcpIpv4Host> {
        Arc::new(NativeDhcpIpv4Host {
            global_ctx: self.global_ctx.clone(),
            operation,
            cancel: self.cancel.clone(),
            nic: self.nic.clone(),
            closed: Arc::new(Notify::new()),
        })
    }
}

struct NativeDhcpIpv4Host {
    global_ctx: ArcGlobalCtx,
    operation: Arc<Mutex<()>>,
    cancel: CancellationToken,
    nic: TunNicState,
    closed: Arc<Notify>,
}

impl NativeDhcpIpv4Host {
    fn ensure_open(&self) -> anyhow::Result<()> {
        if self.cancel.is_cancelled() {
            anyhow::bail!("instance is closing; DHCP update cancelled");
        }
        Ok(())
    }

    async fn apply(&self, next: Option<Ipv4Inet>) -> anyhow::Result<Option<Ipv4Inet>> {
        self.ensure_open()?;
        tokio::select! {
            _ = self.cancel.cancelled() => anyhow::bail!("instance is closing; DHCP update cancelled"),
            _ = self.nic.drain() => {}
        }
        self.ensure_open()?;

        let Some(ip) = next else {
            self.global_ctx.set_ipv4(None);
            return Ok(None);
        };
        self.global_ctx.set_ipv4(Some(ip));
        Ok(Some(ip))
    }
}

#[async_trait::async_trait]
impl DhcpIpv4Host for NativeDhcpIpv4Host {
    fn take_interface_closed(&self) -> bool {
        self.closed.notified().now_or_never().is_some()
    }

    async fn apply_dhcp_ipv4(
        &self,
        _previous: Option<Ipv4Inet>,
        next: Option<Ipv4Inet>,
    ) -> DhcpIpv4ApplyOutcome {
        let permit = self.operation.clone().lock_owned().await;
        let outcome = match self.apply(next).await {
            Ok(actual) => DhcpIpv4ApplyOutcome::applied(actual),
            Err(error) => DhcpIpv4ApplyOutcome::failed(self.global_ctx.get_ipv4(), error),
        };
        outcome.with_permit(DhcpIpv4ApplyPermit::new(permit))
    }

    fn publish_dhcp_ipv4(
        &self,
        previous: Option<Ipv4Inet>,
        requested: Option<Ipv4Inet>,
        actual: Option<Ipv4Inet>,
    ) {
        let event = if requested.is_none() {
            GlobalCtxEvent::DhcpIpv4Conflicted(previous)
        } else {
            GlobalCtxEvent::DhcpIpv4Changed(previous, actual)
        };
        self.global_ctx.issue_event(event);
    }
}
