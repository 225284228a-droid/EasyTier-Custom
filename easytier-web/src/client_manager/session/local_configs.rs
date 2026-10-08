//! Pull-only observations for nodes whose local files own their configuration.
use std::{sync::Weak, time::Duration};

use easytier::proto::{
    api::manage::{
        DeleteNetworkInstanceRequest, ObserveConfigsRequest, PatchPersistedConfigRequest,
        PersistedConfigService, PersistedConfigServiceClientFactory,
        SetNetworkInstanceEnabledRequest, WebClientServiceClientFactory,
    },
    rpc_types::controller::BaseController,
    web::HeartbeatRequest,
};
use tokio::sync::{RwLock, broadcast};

use super::{Session, SessionData, SharedSessionData};
use crate::db::local_config_mirror::LocalConfigMirror;

pub(crate) const REVISION_CAPABILITY: &str = "management:persisted-config-revision-v1";
pub(super) type PersistedClient =
    Box<dyn PersistedConfigService<Controller = BaseController> + Send + Sync>;

#[derive(Debug, thiserror::Error)]
pub(crate) enum LocalConfigError {
    #[error("device is offline; no write was queued")]
    Offline,
    #[error("device has not advertised persisted revision management")]
    Unsupported,
    #[error("configuration changed; refresh before saving again")]
    Conflict(Option<String>),
    #[error("configuration is protected")]
    Protected,
    #[error("invalid configuration patch")]
    Invalid,
    #[error("write outcome is unknown; observe before another save")]
    Unknown,
    #[error("configuration could not be applied")]
    ApplyFailed,
    #[error("configuration observation failed")]
    Observation,
}

pub(crate) fn supports_revision(req: &HeartbeatRequest) -> bool {
    req.support_local_configs
        && req.support_local_config_revision
        && req
            .runtime_capabilities
            .iter()
            .any(|cap| cap == REVISION_CAPABILITY)
}

pub(super) async fn persist_management_mode(data: &SharedSessionData) -> anyhow::Result<()> {
    let data = data.read().await;
    if !data.auth_state.is_authorized() {
        return Ok(());
    }
    let Some((storage, token)) = data.storage.upgrade().zip(data.storage_token.as_ref()) else {
        return Ok(());
    };
    let Some(req) = data.req.as_ref() else {
        return Ok(());
    };
    if storage.owns_authorized_session(token, data.session_epoch) {
        storage
            .db
            .store_local_config_mode(
                token.user_id,
                token.machine_id,
                storage.local_observer_epoch,
                data.session_epoch,
                req.support_local_configs,
            )
            .await?;
    }
    Ok(())
}

/// Callers never retry a mutation through this observer. A timeout can only
/// trigger another read of the node's authoritative state.
pub(super) async fn observe(
    data: &SharedSessionData,
    client: &(dyn PersistedConfigService<Controller = BaseController> + Send + Sync),
) -> anyhow::Result<LocalConfigMirror> {
    let (storage, token, session_epoch, binding_version, req) = {
        let data = data.read().await;
        anyhow::ensure!(data.auth_state.is_authorized(), "local_config_offline");
        let req = data
            .req
            .as_ref()
            .filter(|req| supports_revision(req))
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("local_config_unsupported"))?;
        let token = data
            .storage_token
            .clone()
            .ok_or_else(|| anyhow::anyhow!("local_config_offline"))?;
        let storage = data
            .storage
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("local_config_offline"))?;
        (
            storage,
            token,
            data.session_epoch,
            data.binding_version,
            req,
        )
    };
    let lock = storage.runtime_mutation_lock((token.user_id, token.machine_id));
    let _mutation = lock.lock().await;
    anyhow::ensure!(
        storage.owns_authorized_session(&token, session_epoch),
        "local_config_session_changed"
    );
    let snapshot = tokio::time::timeout(
        Duration::from_secs(5),
        client.observe_configs(BaseController::default(), ObserveConfigsRequest {}),
    )
    .await
    .map_err(|_| anyhow::anyhow!("local_config_read_timeout"))??;
    let capabilities = {
        let current = data.read().await;
        anyhow::ensure!(
            current.binding_version == binding_version
                && current.auth_state.is_authorized()
                && current.req.as_ref().is_some_and(|heartbeat| {
                    supports_revision(heartbeat)
                        && heartbeat.user_token == req.user_token
                        && heartbeat.machine_id == req.machine_id
                        && heartbeat.local_config_catalog_epoch == snapshot.catalog_epoch
                        && heartbeat.local_config_catalog_generation <= snapshot.catalog_generation
                })
                && storage.owns_authorized_session(&token, session_epoch),
            "local_config_session_changed"
        );
        current.req.as_ref().unwrap().runtime_capabilities.clone()
    };
    let accepted = storage
        .db
        .store_local_config_snapshot(
            token.user_id,
            token.machine_id,
            storage.local_observer_epoch,
            session_epoch,
            &snapshot,
            &capabilities,
        )
        .await?;
    anyhow::ensure!(accepted, "local_config_stale_observation");
    Ok(LocalConfigMirror {
        machine_id: token.machine_id,
        snapshot,
        capabilities,
        observed_at: chrono::Utc::now().to_rfc3339(),
        online: true,
        stale: false,
    })
}

pub(super) async fn worker(
    data: Weak<RwLock<SessionData>>,
    client: PersistedClient,
    mut heartbeats: broadcast::Receiver<HeartbeatRequest>,
) {
    let mut full_check = tokio::time::interval(Duration::from_secs(30));
    full_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut observed = None;
    loop {
        let full = tokio::select! {
            _ = full_check.tick() => true,
            event = heartbeats.recv() => {
                match event {
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => false,
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        };
        let Some(data) = data.upgrade() else { return };
        // Remember even legacy custom nodes. Losing their live capability flag
        // must never send them into the official offline publication queue.
        if let Err(error) = persist_management_mode(&data).await {
            tracing::warn!(%error, "could not persist node configuration management mode");
        }
        let revision = {
            let data = data.read().await;
            data.req
                .as_ref()
                .filter(|req| supports_revision(req))
                .map(|req| {
                    (
                        req.local_config_catalog_epoch.clone(),
                        req.local_config_catalog_generation,
                    )
                })
        };
        let Some(revision) = revision else { continue };
        if !full && observed.as_ref() == Some(&revision) {
            continue;
        }
        match observe(&data, client.as_ref()).await {
            Ok(mirror) => {
                observed = Some((
                    mirror.snapshot.catalog_epoch,
                    mirror.snapshot.catalog_generation,
                ))
            }
            Err(error) => tracing::debug!(%error, "local config snapshot refresh failed"),
        }
    }
}

impl Session {
    pub(super) fn persisted_client(&self) -> PersistedClient {
        self.scoped_client::<PersistedConfigServiceClientFactory<BaseController>>()
    }

    pub(crate) async fn observe_local_configs(&self) -> anyhow::Result<LocalConfigMirror> {
        observe(&self.data, self.persisted_client().as_ref()).await
    }

    pub(crate) async fn has_local_revision_capability(&self) -> bool {
        let data = self.data.read().await;
        data.auth_state.is_authorized() && data.req.as_ref().is_some_and(supports_revision)
    }

    pub(crate) async fn local_catalog_binding(&self) -> Option<(String, u64, Vec<String>)> {
        let data = self.data.read().await;
        if !data.auth_state.is_authorized() {
            return None;
        }
        let req = data.req.as_ref().filter(|req| supports_revision(req))?;
        Some((
            req.local_config_catalog_epoch.clone(),
            req.local_config_catalog_generation,
            req.runtime_capabilities.clone(),
        ))
    }

    pub(crate) async fn patch_local_config(
        &self,
        request: PatchPersistedConfigRequest,
    ) -> Result<LocalConfigMirror, LocalConfigError> {
        let (storage, token, session_epoch, binding, capabilities) = {
            let data = self.data.read().await;
            if !data.auth_state.is_authorized() {
                return Err(LocalConfigError::Offline);
            }
            let req = data
                .req
                .as_ref()
                .filter(|req| supports_revision(req))
                .ok_or(LocalConfigError::Unsupported)?;
            (
                data.storage.upgrade().ok_or(LocalConfigError::Offline)?,
                data.storage_token
                    .clone()
                    .ok_or(LocalConfigError::Offline)?,
                data.session_epoch,
                data.binding_version,
                req.runtime_capabilities.clone(),
            )
        };
        validate_capability_mask(&request, &capabilities)?;
        let lock = storage.runtime_mutation_lock((token.user_id, token.machine_id));
        let mutation = lock.lock().await;
        if !storage.owns_authorized_session(&token, session_epoch) {
            return Err(LocalConfigError::Offline);
        }
        let response = tokio::time::timeout(
            Duration::from_secs(10),
            self.persisted_client()
                .patch_config(BaseController::default(), request),
        )
        .await;
        let still_owned = {
            let data = self.data.read().await;
            data.binding_version == binding
                && data.auth_state.is_authorized()
                && storage.owns_authorized_session(&token, session_epoch)
        };
        drop(mutation);
        if !still_owned {
            return Err(LocalConfigError::Unknown);
        }
        // A read refresh is safe even when the write timed out. Never resubmit it.
        let refreshed = self.observe_local_configs().await;
        let response = response
            .map_err(|_| LocalConfigError::Unknown)?
            .map_err(|_| LocalConfigError::Unknown)?;
        match response.status {
            0 => refreshed.map_err(|_| LocalConfigError::Unknown),
            1 => Err(LocalConfigError::Conflict(
                response.entry.map(|entry| entry.revision),
            )),
            2 => Err(LocalConfigError::Protected),
            3 | 4 => Err(LocalConfigError::Invalid),
            5 => Err(LocalConfigError::ApplyFailed),
            _ => Err(LocalConfigError::Unsupported),
        }
    }

    pub(crate) async fn mutate_local_config_lifecycle(
        &self,
        instance: uuid::Uuid,
        expected_revision: String,
        enabled: Option<bool>,
    ) -> Result<LocalConfigMirror, LocalConfigError> {
        if !self.has_local_revision_capability().await {
            return Err(LocalConfigError::Unsupported);
        }
        let (storage, token, epoch) = {
            let data = self.data.read().await;
            (
                data.storage.upgrade().ok_or(LocalConfigError::Offline)?,
                data.storage_token
                    .clone()
                    .ok_or(LocalConfigError::Offline)?,
                data.session_epoch,
            )
        };
        let lock = storage.runtime_mutation_lock((token.user_id, token.machine_id));
        let mutation = lock.lock().await;
        if !storage.owns_authorized_session(&token, epoch) {
            return Err(LocalConfigError::Offline);
        }
        let client = self.scoped_client::<WebClientServiceClientFactory<BaseController>>();
        let result = tokio::time::timeout(Duration::from_secs(10), async {
            match enabled {
                Some(enabled) => client
                    .set_network_instance_enabled(
                        BaseController::default(),
                        SetNetworkInstanceEnabledRequest {
                            inst_id: Some(instance.into()),
                            enabled,
                            expected_revision: Some(expected_revision),
                        },
                    )
                    .await
                    .map(|_| ()),
                None => client
                    .delete_network_instance(
                        BaseController::default(),
                        DeleteNetworkInstanceRequest {
                            inst_ids: vec![instance.into()],
                            expected_revision: Some(expected_revision),
                        },
                    )
                    .await
                    .and_then(|response| {
                        if response.remain_inst_ids.contains(&instance.into()) {
                            Err(anyhow::anyhow!("configuration could not be removed").into())
                        } else {
                            Ok(())
                        }
                    }),
            }
        })
        .await;
        let still_owned = storage.owns_authorized_session(&token, epoch);
        drop(mutation);
        if !still_owned {
            return Err(LocalConfigError::Unknown);
        }
        let refreshed = self.observe_local_configs().await;
        match result {
            Ok(Ok(())) => refreshed.map_err(|_| LocalConfigError::Unknown),
            Ok(Err(error)) if error.to_string().contains("local_config_revision_conflict") => {
                Err(LocalConfigError::Conflict(None))
            }
            Ok(Err(error))
                if error.to_string().contains("read-only")
                    || error.to_string().contains("protected") =>
            {
                Err(LocalConfigError::Protected)
            }
            Ok(Err(_)) => Err(LocalConfigError::ApplyFailed),
            Err(_) => Err(LocalConfigError::Unknown),
        }
    }
}

fn validate_capability_mask(
    request: &PatchPersistedConfigRequest,
    caps: &[String],
) -> Result<(), LocalConfigError> {
    const EXTENSIONS: &[&str] = &[
        "sni",
        "enable_bbr",
        "p2p_prefer_protocol",
        "only_use_wss_http3_for_hole_punching",
        "prefer_wss_http3_for_p2p",
        "disable_wss_http3_for_p2p",
        "close_redundant_conns_when_disguised",
    ];
    for path in &request.field_mask {
        if EXTENSIONS.contains(&path.as_str())
            && !caps.iter().any(|cap| cap == &format!("config:{path}"))
        {
            return Err(LocalConfigError::Unsupported);
        }
    }
    if request
        .field_mask
        .iter()
        .any(|path| path == "p2p_prefer_protocol")
        && request
            .config
            .as_ref()
            .and_then(|config| config.p2p_prefer_protocol.as_deref())
            == Some("http3")
        && !caps.iter().any(|cap| cap == "transport:http3-framed-v1")
    {
        return Err(LocalConfigError::Unsupported);
    }
    if let Some(config) = request.config.as_ref() {
        let mut urls = Vec::new();
        for mask in &request.field_mask {
            match mask.as_str() {
                "peer_urls" => urls.extend(config.peer_urls.iter().map(String::as_str)),
                "peers" => urls.extend(config.peers.iter().map(|peer| peer.uri.as_str())),
                "listener_urls" => urls.extend(config.listener_urls.iter().map(String::as_str)),
                "mapped_listeners" => {
                    urls.extend(config.mapped_listeners.iter().map(String::as_str))
                }
                "public_server_url" => urls.extend(
                    config
                        .public_server_url
                        .as_deref()
                        .filter(|uri| !uri.trim().is_empty()),
                ),
                _ => (),
            }
        }
        for uri in urls {
            let Ok(url) = url::Url::parse(uri) else {
                return Err(LocalConfigError::Invalid);
            };
            let capability = match url.scheme() {
                "ws" => "transport:ws",
                "wss" => "transport:wss",
                "http3" => "transport:http3-framed-v1",
                _ => continue,
            };
            if !caps.iter().any(|cap| cap == capability) {
                return Err(LocalConfigError::Unsupported);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "local_config_tests.rs"]
mod tests;
