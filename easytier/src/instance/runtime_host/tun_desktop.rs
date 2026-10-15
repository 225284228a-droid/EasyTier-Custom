use std::{sync::Arc, time::Duration};

use anyhow::Context as _;
use easytier_core::{
    gateway::dhcp::DhcpHost, host::packet::HostPacketReceiver, instance::CorePacketPlane,
};
use tokio::{
    sync::{Mutex, Notify, oneshot},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use super::tun_common::{NativeDhcpHost, TunNicState};
use crate::{
    common::{error::Error, global_ctx::ArcGlobalCtx},
    instance::{shared_virtual_nic::ArcSharedVirtualNicRegistry, virtual_nic::NicCtx},
};

pub(super) struct NativeTunRuntime {
    global_ctx: ArcGlobalCtx,
    cancel: CancellationToken,
    nic: TunNicState,
    shared_virtual_nic_registry: ArcSharedVirtualNicRegistry,
    static_ip_task: Mutex<Option<JoinHandle<()>>>,
}

impl NativeTunRuntime {
    fn report_capture_unavailable(&self, reason: &str) {
        self.global_ctx.vpn_route_status().update(
            easytier_core::peers::vpn_policy::VpnHostRouteState {
                activation_error: self
                    .global_ctx
                    .config
                    .get_vpn()
                    .client_active()
                    .then(|| reason.to_owned()),
                ..Default::default()
            },
        );
    }

    pub(super) fn new(
        global_ctx: ArcGlobalCtx,
        cancel: CancellationToken,
        shared_virtual_nic_registry: ArcSharedVirtualNicRegistry,
    ) -> Self {
        Self {
            global_ctx,
            cancel,
            nic: TunNicState::empty(),
            shared_virtual_nic_registry,
            static_ip_task: Mutex::new(None),
        }
    }

    pub(super) fn install_packet_receiver(
        &self,
        receiver: HostPacketReceiver,
    ) -> anyhow::Result<()> {
        self.nic.install_receiver(receiver)
    }

    fn report_static_ip_cancelled(output: &mut Option<oneshot::Sender<Result<(), Error>>>) {
        if let Some(output) = output.take() {
            let _ = output.send(Err(anyhow::anyhow!(
                "instance is closing; static IP setup cancelled"
            )
            .into()));
        }
    }

    async fn start_static_ip(&self, packet_plane: Arc<CorePacketPlane>) -> anyhow::Result<()> {
        let ipv4 = self.global_ctx.get_ipv4();
        let ipv6 = self.global_ctx.get_ipv6();
        if ipv4.is_none() && ipv6.is_none() {
            return Ok(());
        }

        let nic_state = self.nic.clone();
        let cancel = self.cancel.clone();
        let global_ctx = self.global_ctx.clone();
        let receiver = self.nic.receiver();
        let shared_virtual_nic_registry = self.shared_virtual_nic_registry.clone();
        let (output, first_round) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut output = Some(output);
            loop {
                if cancel.is_cancelled() {
                    Self::report_static_ip_cancelled(&mut output);
                    return;
                }
                nic_state.stop().await;
                let closed = Arc::new(Notify::new());
                let mut nic = NicCtx::new(
                    global_ctx.clone(),
                    packet_plane.clone(),
                    receiver.clone(),
                    closed.clone(),
                    shared_virtual_nic_registry.clone(),
                )
                .await;
                let result = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        nic.shutdown().await;
                        Self::report_static_ip_cancelled(&mut output);
                        return;
                    }
                    result = nic.run(ipv4, ipv6) => result,
                };
                if let Err(error) = result {
                    if let Some(output) = output.take() {
                        let _ = output.send(Err(error));
                        return;
                    }
                    tracing::error!(?error, "failed to create native interface");
                    tokio::select! {
                        _ = cancel.cancelled() => return,
                        _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                    }
                    continue;
                }

                if let Err(error) = nic_state.install(None, nic).await {
                    if let Some(output) = output.take() {
                        let _ = output.send(Err(error.into()));
                    }
                    return;
                }
                if let Some(output) = output.take() {
                    let _ = output.send(Ok(()));
                }
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = closed.notified() => {}
                }
            }
        });
        self.static_ip_task.lock().await.replace(task);
        first_round
            .await
            .context("static IP setup task stopped")??;
        Ok(())
    }

    pub(super) async fn prepare(&self, packet_plane: Arc<CorePacketPlane>) -> anyhow::Result<()> {
        self.nic.drain().await;
        if !self.global_ctx.get_flags().no_tun
            && !self.global_ctx.config.get_dhcp()
            && !self.global_ctx.config.get_dhcpv6()
        {
            self.start_static_ip(packet_plane).await?;
        } else if self.global_ctx.get_flags().no_tun {
            self.report_capture_unavailable(
                "VPN capture unavailable: no_tun disables the native interface",
            );
        }
        Ok(())
    }

    pub(super) async fn shutdown(&self) {
        if let Some(task) = self.static_ip_task.lock().await.take() {
            let _ = task.await;
        }
        self.nic.stop().await;
    }

    pub(super) fn attach_fd(&self, _fd: i32) -> anyhow::Result<()> {
        anyhow::bail!("external TUN attachment is only supported on mobile Hosts")
    }

    pub(super) fn dhcp_host(
        &self,
        operation: Arc<Mutex<()>>,
        packet_plane: Arc<CorePacketPlane>,
    ) -> Arc<dyn DhcpHost> {
        Arc::new(NativeDhcpHost {
            global_ctx: self.global_ctx.clone(),
            operation,
            cancel: self.cancel.clone(),
            nic: self.nic.clone(),
            closed: Arc::new(Notify::new()),
            packet_plane,
            shared_virtual_nic_registry: self.shared_virtual_nic_registry.clone(),
        })
    }
}
