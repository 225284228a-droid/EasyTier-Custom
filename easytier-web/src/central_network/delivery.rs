//! Writes to members' config files over connections that may drop. A write
//! carries the revision it was computed from. One that timed out, lost its
//! connection or met a newer revision is computed again from a fresh read
//! and tried again, a few times. Only the fields that still differ are
//! written, so a write that landed unseen is not repeated. Different devices
//! are written at the same time; one device's writes stay in order.

use std::{collections::BTreeMap, future::Future, time::Duration};

use easytier::proto::api::manage::PatchPersistedConfigRequest;
use uuid::Uuid;

use crate::{
    client_manager::{
        ClientManager,
        session::local_configs::{LocalConfigError, LocalConfigView},
    },
    db::UserIdInDb,
};

use super::core_sync::instance_entry;

/// The pauses before the second and third attempt of a write.
const BACKOFF: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(3)];

/// Failures after which the same write may still succeed.
fn transient(error: &LocalConfigError) -> bool {
    matches!(
        error,
        LocalConfigError::Offline
            | LocalConfigError::Unknown
            | LocalConfigError::Observation
            | LocalConfigError::Conflict(_)
    )
}

/// What an attempt does after reading the device.
enum Step {
    /// Nothing is left to do.
    Done,
    Patch(Box<PatchPersistedConfigRequest>),
    SetEnabled {
        instance: Uuid,
        revision: String,
        enabled: bool,
    },
}

/// What a write needs from one device.
trait Device {
    /// The device's config directory, read now.
    async fn read(&self) -> Result<LocalConfigView, LocalConfigError>;
    async fn patch(&self, request: PatchPersistedConfigRequest) -> Result<(), LocalConfigError>;
    async fn set_enabled(
        &self,
        instance: Uuid,
        revision: String,
        enabled: bool,
    ) -> Result<(), LocalConfigError>;
}

/// A device through its connection to this console.
struct Connected<'a> {
    manager: &'a ClientManager,
    user_id: UserIdInDb,
    device_id: Uuid,
}

impl Device for Connected<'_> {
    async fn read(&self) -> Result<LocalConfigView, LocalConfigError> {
        self.manager
            .observe_local_configs(self.user_id, self.device_id)
            .await
    }

    async fn patch(&self, request: PatchPersistedConfigRequest) -> Result<(), LocalConfigError> {
        self.manager
            .patch_local_config(self.user_id, self.device_id, request)
            .await
            .map(|_| ())
    }

    async fn set_enabled(
        &self,
        instance: Uuid,
        revision: String,
        enabled: bool,
    ) -> Result<(), LocalConfigError> {
        self.manager
            .mutate_local_config_lifecycle(
                self.user_id,
                self.device_id,
                instance,
                revision,
                Some(enabled),
            )
            .await
            .map(|_| ())
    }
}

/// One write, tried again after a transient failure. `plan` reads what to
/// do from the latest read; an error from it ends the attempts. A write
/// whose outcome was unknown counts as written once a later read leaves
/// nothing to do. Returns whether anything was written.
async fn retry(
    device: &impl Device,
    backoff: &[Duration],
    mut first: Option<LocalConfigView>,
    mut plan: impl FnMut(&LocalConfigView) -> Result<Step, LocalConfigError>,
) -> Result<bool, LocalConfigError> {
    let mut written = false;
    let mut last = LocalConfigError::Unknown;
    for pause in std::iter::once(None).chain(backoff.iter().map(Some)) {
        if let Some(pause) = pause {
            tokio::time::sleep(*pause).await;
        }
        let view = match first.take() {
            Some(view) => view,
            None => match device.read().await {
                Ok(view) => view,
                Err(error) if transient(&error) => {
                    last = error;
                    continue;
                }
                Err(error) => return Err(error),
            },
        };
        let outcome = match plan(&view)? {
            Step::Done => return Ok(written),
            Step::Patch(request) => device.patch(*request).await,
            Step::SetEnabled {
                instance,
                revision,
                enabled,
            } => device.set_enabled(instance, revision, enabled).await,
        };
        match outcome {
            Ok(()) => return Ok(true),
            Err(error) if transient(&error) => {
                written |= matches!(error, LocalConfigError::Unknown);
                last = error;
            }
            Err(error) => return Err(error),
        }
    }
    Err(last)
}

async fn patch_with(
    device: &impl Device,
    backoff: &[Duration],
    first: Option<LocalConfigView>,
    mut request: impl FnMut(
        &LocalConfigView,
    ) -> Result<Option<PatchPersistedConfigRequest>, LocalConfigError>,
) -> Result<bool, LocalConfigError> {
    retry(device, backoff, first, |view| {
        Ok(request(view)?.map_or(Step::Done, |request| Step::Patch(Box::new(request))))
    })
    .await
}

/// Write `request(view)` to the device, built again from the latest read
/// before every attempt. `None` means nothing is left to write; an error
/// from `request` ends the attempts. Returns whether anything was written.
pub(super) async fn patch(
    manager: &ClientManager,
    user_id: UserIdInDb,
    device_id: Uuid,
    first: Option<LocalConfigView>,
    request: impl FnMut(
        &LocalConfigView,
    ) -> Result<Option<PatchPersistedConfigRequest>, LocalConfigError>,
) -> Result<bool, LocalConfigError> {
    let device = Connected {
        manager,
        user_id,
        device_id,
    };
    patch_with(&device, &BACKOFF, first, request).await
}

async fn set_enabled_with(
    device: &impl Device,
    backoff: &[Duration],
    instance_id: &str,
    first: Option<LocalConfigView>,
    enabled: bool,
) -> Result<bool, LocalConfigError> {
    retry(device, backoff, first, |view| {
        let entry = instance_entry(view, instance_id)
            .ok_or_else(|| LocalConfigError::NotFound(instance_id.to_owned()))?;
        if entry.enabled == enabled {
            return Ok(Step::Done);
        }
        let instance = entry
            .inst_id
            .map(Uuid::from)
            .ok_or_else(|| LocalConfigError::Invalid("configuration has no identity".to_owned()))?;
        Ok(Step::SetEnabled {
            instance,
            revision: entry.revision,
            enabled,
        })
    })
    .await
}

/// Start or stop the member's network, retried like [`patch`]. Returns
/// whether its state changed.
pub(super) async fn set_enabled(
    manager: &ClientManager,
    user_id: UserIdInDb,
    device_id: Uuid,
    instance_id: &str,
    first: Option<LocalConfigView>,
    enabled: bool,
) -> Result<bool, LocalConfigError> {
    let device = Connected {
        manager,
        user_id,
        device_id,
    };
    set_enabled_with(&device, &BACKOFF, instance_id, first, enabled).await
}

/// Run `work` for every device's items at the same time; a device's items
/// keep their order within one call.
pub(super) async fn per_device<T, R, Fut>(
    items: impl IntoIterator<Item = (Uuid, T)>,
    work: impl Fn(Uuid, Vec<T>) -> Fut,
) -> Vec<R>
where
    Fut: Future<Output = Vec<R>>,
{
    let mut devices: BTreeMap<Uuid, Vec<T>> = BTreeMap::new();
    for (device_id, item) in items {
        devices.entry(device_id).or_default().push(item);
    }
    futures::future::join_all(
        devices
            .into_iter()
            .map(|(device_id, items)| work(device_id, items)),
    )
    .await
    .into_iter()
    .flatten()
    .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use easytier::proto::api::manage::{
        NetworkConfig, ObserveConfigsResponse, PersistedConfigEntry,
    };

    use super::*;

    const NO_PAUSE: [Duration; 2] = [Duration::ZERO, Duration::ZERO];

    /// A node whose answers are scripted: each write or state change takes
    /// the next outcome; `landed` says whether a failed one still applied.
    struct Node {
        instance: Uuid,
        state: Mutex<(NetworkConfig, u64, bool)>,
        outcomes: Mutex<Vec<(Option<LocalConfigError>, bool)>>,
        calls: Mutex<usize>,
    }

    impl Node {
        fn new(outcomes: Vec<(Option<LocalConfigError>, bool)>) -> Self {
            Self {
                instance: Uuid::from_u128(7),
                state: Mutex::new((NetworkConfig::default(), 1, true)),
                outcomes: Mutex::new(outcomes),
                calls: Mutex::new(0),
            }
        }

        fn view(&self) -> LocalConfigView {
            let (config, revision, enabled) = self.state.lock().unwrap().clone();
            LocalConfigView {
                machine_id: Uuid::nil(),
                snapshot: ObserveConfigsResponse {
                    entries: vec![PersistedConfigEntry {
                        inst_id: Some(self.instance.into()),
                        revision: revision.to_string(),
                        config: Some(config),
                        enabled,
                        status: "ready".to_owned(),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                capabilities: Vec::new(),
                observed_at: String::new(),
                online: true,
                stale: false,
            }
        }

        /// Apply `change` when the revision matches and the script lets it.
        fn answer(
            &self,
            expected: &str,
            change: impl FnOnce(&mut (NetworkConfig, u64, bool)),
        ) -> Result<(), LocalConfigError> {
            *self.calls.lock().unwrap() += 1;
            let mut state = self.state.lock().unwrap();
            if expected != state.1.to_string() {
                return Err(LocalConfigError::Conflict(None));
            }
            let mut outcomes = self.outcomes.lock().unwrap();
            let (error, landed) = if outcomes.is_empty() {
                (None, true)
            } else {
                outcomes.remove(0)
            };
            if landed {
                change(&mut state);
                state.1 += 1;
            }
            error.map_or(Ok(()), Err)
        }

        fn calls(&self) -> usize {
            *self.calls.lock().unwrap()
        }
    }

    impl Device for Node {
        async fn read(&self) -> Result<LocalConfigView, LocalConfigError> {
            Ok(self.view())
        }

        async fn patch(
            &self,
            request: PatchPersistedConfigRequest,
        ) -> Result<(), LocalConfigError> {
            let mtu = request.config.and_then(|config| config.mtu);
            self.answer(&request.expected_revision, |state| state.0.mtu = mtu)
        }

        async fn set_enabled(
            &self,
            _instance: Uuid,
            revision: String,
            enabled: bool,
        ) -> Result<(), LocalConfigError> {
            self.answer(&revision, |state| state.2 = enabled)
        }
    }

    /// Set the MTU to 1300 unless it is already.
    fn mtu_request(
        view: &LocalConfigView,
    ) -> Result<Option<PatchPersistedConfigRequest>, LocalConfigError> {
        let entry = &view.snapshot.entries[0];
        Ok((entry.config.as_ref().unwrap().mtu != Some(1300)).then(|| {
            PatchPersistedConfigRequest {
                inst_id: entry.inst_id,
                expected_revision: entry.revision.clone(),
                config: Some(NetworkConfig {
                    mtu: Some(1300),
                    ..Default::default()
                }),
                field_mask: vec!["mtu".to_owned()],
                toml: None,
            }
        }))
    }

    #[tokio::test]
    async fn a_timed_out_write_that_landed_is_not_sent_again() {
        let node = Node::new(vec![(Some(LocalConfigError::Unknown), true)]);
        let written = patch_with(&node, &NO_PAUSE, Some(node.view()), mtu_request).await;
        assert!(matches!(written, Ok(true)));
        assert_eq!(node.calls(), 1);
        assert_eq!(
            node.view().snapshot.entries[0].config.as_ref().unwrap().mtu,
            Some(1300)
        );
    }

    #[tokio::test]
    async fn a_lost_write_is_sent_again_from_a_fresh_read() {
        let node = Node::new(vec![
            (Some(LocalConfigError::Unknown), false),
            (Some(LocalConfigError::Offline), false),
        ]);
        let written = patch_with(&node, &NO_PAUSE, Some(node.view()), mtu_request).await;
        assert!(matches!(written, Ok(true)));
        assert_eq!(node.calls(), 3);
    }

    #[tokio::test]
    async fn a_stale_read_is_read_again_instead_of_reporting_a_conflict() {
        let node = Node::new(Vec::new());
        let stale = node.view();
        // The node changed after the console's read.
        node.state.lock().unwrap().1 += 1;
        let written = patch_with(&node, &NO_PAUSE, Some(stale), mtu_request).await;
        assert!(matches!(written, Ok(true)));
        assert_eq!(node.calls(), 2);
    }

    #[tokio::test]
    async fn lasting_and_final_failures_stop() {
        let lost = || (Some(LocalConfigError::Unknown), false);
        let node = Node::new(vec![lost(), lost(), lost()]);
        let written = patch_with(&node, &NO_PAUSE, Some(node.view()), mtu_request).await;
        assert!(matches!(written, Err(LocalConfigError::Unknown)));
        assert_eq!(node.calls(), 3, "three attempts in all");

        let node = Node::new(vec![(Some(LocalConfigError::Protected), false)]);
        let written = patch_with(&node, &NO_PAUSE, Some(node.view()), mtu_request).await;
        assert!(matches!(written, Err(LocalConfigError::Protected)));
        assert_eq!(node.calls(), 1);

        // A request that refuses, such as another console's newer change.
        let node = Node::new(Vec::new());
        let written = patch_with(&node, &NO_PAUSE, Some(node.view()), |_| {
            Err(LocalConfigError::Conflict(None))
        })
        .await;
        assert!(matches!(written, Err(LocalConfigError::Conflict(_))));
        assert_eq!(node.calls(), 0);
    }

    #[tokio::test]
    async fn state_changes_retry_and_skip_what_already_holds() {
        let node = Node::new(vec![(Some(LocalConfigError::Unknown), false)]);
        let instance = node.instance.to_string();
        let stopped = set_enabled_with(&node, &NO_PAUSE, &instance, None, false).await;
        assert!(matches!(stopped, Ok(true)));
        assert_eq!(node.calls(), 2);
        assert!(!node.view().snapshot.entries[0].enabled);
        let again = set_enabled_with(&node, &NO_PAUSE, &instance, None, false).await;
        assert!(matches!(again, Ok(false)));
        assert_eq!(node.calls(), 2);
    }

    #[tokio::test]
    async fn devices_run_together_and_keep_their_own_order() {
        let started = std::sync::Arc::new(Mutex::new(Vec::new()));
        let devices = [Uuid::from_u128(1), Uuid::from_u128(2)];
        let items = [(devices[0], 1), (devices[1], 10), (devices[0], 2)];
        let begin = tokio::time::Instant::now();
        let done = per_device(items, |device_id, items| {
            let started = started.clone();
            async move {
                let mut done = Vec::new();
                for item in items {
                    started.lock().unwrap().push(item);
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    done.push((device_id, item));
                }
                done
            }
        })
        .await;
        // Two items on one device and one on the other take two steps, not three.
        assert!(begin.elapsed() < Duration::from_millis(550));
        assert_eq!(done, [(devices[0], 1), (devices[0], 2), (devices[1], 10)]);
        let started = started.lock().unwrap();
        let first = started.iter().position(|item| *item == 1).unwrap();
        let second = started.iter().position(|item| *item == 2).unwrap();
        assert!(first < second);
    }

    #[test]
    fn only_failures_that_may_pass_are_retried() {
        assert!(transient(&LocalConfigError::Unknown));
        assert!(transient(&LocalConfigError::Offline));
        assert!(transient(&LocalConfigError::Observation));
        assert!(transient(&LocalConfigError::Conflict(None)));
        assert!(!transient(&LocalConfigError::Protected));
        assert!(!transient(&LocalConfigError::Invalid(String::new())));
        assert!(!transient(&LocalConfigError::NotFound(String::new())));
        assert!(!transient(&LocalConfigError::ApplyFailed));
    }
}
