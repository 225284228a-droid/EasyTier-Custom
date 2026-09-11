use std::sync::Arc;

use async_trait::async_trait;
use easytier_core::{
    connectivity::{
        protocol::{
            ClientProtocolUpgrader, ServerProtocolAdmission, ServerProtocolUpgrade,
            ServerProtocolUpgrader,
        },
        transport::ConnectedTransport,
    },
    socket::udp::UdpSession,
    tunnel::Tunnel,
};

use crate::{
    common::global_ctx::ArcGlobalCtx,
    socket::tcp::RuntimeTcpSocket,
    tunnel::quic::{QuicAcceptedSession, upgrade_connected},
};

use super::{ClientAdapter, ServerAdapter};

#[derive(Default)]
struct QuicAdapter {
    enable_bbr: bool,
}

pub(super) fn client_adapter(global_ctx: &ArcGlobalCtx) -> ClientAdapter {
    Arc::new(QuicAdapter {
        enable_bbr: global_ctx.get_flags().enable_bbr,
    })
}

pub(super) fn server_adapter(global_ctx: &ArcGlobalCtx) -> ServerAdapter {
    Arc::new(QuicAdapter {
        enable_bbr: global_ctx.get_flags().enable_bbr,
    })
}

#[async_trait]
impl ClientProtocolUpgrader<RuntimeTcpSocket> for QuicAdapter {
    fn supports_scheme(&self, scheme: &str) -> bool {
        scheme == "quic"
    }

    async fn upgrade_client(
        &self,
        connected: ConnectedTransport<RuntimeTcpSocket>,
        requested_url: url::Url,
    ) -> anyhow::Result<Box<dyn Tunnel>> {
        let ConnectedTransport::Udp(session) = connected else {
            anyhow::bail!("QUIC protocol requires a UDP session");
        };
        Ok(upgrade_connected(session, requested_url, self.enable_bbr).await?)
    }
}

#[async_trait]
impl ServerProtocolUpgrader<RuntimeTcpSocket> for QuicAdapter {
    fn supports_scheme(&self, scheme: &str) -> bool {
        scheme == "quic"
    }

    async fn upgrade_tcp(
        &self,
        _socket: RuntimeTcpSocket,
        _local_url: url::Url,
    ) -> anyhow::Result<ServerProtocolUpgrade> {
        anyhow::bail!("unsupported native TCP server protocol upgrader: quic")
    }

    async fn upgrade_udp(
        &self,
        session: UdpSession,
        local_url: url::Url,
        admission: Option<ServerProtocolAdmission>,
    ) -> anyhow::Result<ServerProtocolUpgrade> {
        let admission =
            admission.ok_or_else(|| anyhow::anyhow!("QUIC server admission permit is missing"))?;
        Ok(ServerProtocolUpgrade::Acceptor(Box::new(
            QuicAcceptedSession::new(session, local_url, admission, self.enable_bbr)?,
        )))
    }

    async fn upgrade_byte_stream(
        &self,
        _socket: RuntimeTcpSocket,
        _local_url: url::Url,
        _remote_url: Option<url::Url>,
    ) -> anyhow::Result<ServerProtocolUpgrade> {
        anyhow::bail!("unsupported native byte-stream server protocol upgrader: quic")
    }
}
