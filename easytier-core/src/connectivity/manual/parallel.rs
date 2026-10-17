//! Expand an already authenticated manual endpoint without disturbing its
//! ordinary reconnect/removal lifecycle. The peer owns slot reservations.

use super::*;

tokio::task_local! { static EXPECTED_PEER: PeerId; }

pub(super) fn expected_peer_id() -> Option<PeerId> {
    EXPECTED_PEER.try_with(|peer| *peer).ok()
}

pub(super) struct RetryState {
    failures: u8,
    next_attempt: Instant,
}

fn retry_delay(failures: u8) -> Duration {
    Duration::from_secs((1u64 << failures.saturating_sub(1).min(5)).min(30))
}

pub(super) fn schedule<H>(
    data: &Arc<ManualConnectorData<H>>,
    tasks: &mut JoinSet<(Url, anyhow::Result<()>)>,
) where
    H: ManualConnectorHost,
{
    let Some(manager) = data.peer_manager.upgrade() else {
        return;
    };
    let candidates = {
        let _guard = data.state.state_lock.lock().unwrap();
        data.state
            .connectors
            .iter()
            .filter_map(|url| {
                if url.scheme() == "ring" || data.state.removed.contains(url.key()) {
                    return None;
                }
                let peer = data
                    .state
                    .last_connected_peers
                    .get(url.key())
                    .map(|id| *id)?;
                if data
                    .state
                    .parallel_retries
                    .get(url.key())
                    .is_some_and(|retry| Instant::now() < retry.next_attempt)
                {
                    return None;
                }
                Some((url.key().clone(), peer))
            })
            .collect::<Vec<_>>()
    };
    for (url, peer_id) in candidates {
        let local = manager.get_peer_map();
        let map = if local.has_peer(peer_id) {
            local
        } else {
            manager.get_foreign_network_client().get_peer_map()
        };
        // Only the surviving anchor's original client dials. Other manual
        // URLs remain available for the existing failover policy.
        if map.parallel_manual_url(peer_id).as_ref() != Some(&url) {
            continue;
        }
        while let Some(reservation) = map.reserve_parallel_slot(peer_id) {
            let task_data = data.clone();
            let task_url = url.clone();
            tasks.spawn(async move {
                let result = reservation
                    .scope(EXPECTED_PEER.scope(peer_id, async {
                        // A URL removed before this future is polled must not
                        // start another socket or reappear in the registry.
                        if !task_data.state.connectors.contains(&task_url) {
                            anyhow::bail!("manual connector removed before parallel dial");
                        }
                        reconnect(task_data.clone(), task_url.clone()).await
                    }))
                    .await;
                if result.is_ok() {
                    task_data.state.parallel_retries.remove(&task_url);
                } else if task_data.state.connectors.contains(&task_url) {
                    let mut retry = task_data
                        .state
                        .parallel_retries
                        .entry(task_url.clone())
                        .or_insert_with(|| RetryState {
                            failures: 0,
                            next_attempt: Instant::now(),
                        });
                    retry.failures = retry.failures.saturating_add(1);
                    retry.next_attempt = Instant::now() + retry_delay(retry.failures);
                }
                // Keep the reservation until handshake and admission finish.
                drop(reservation);
                (task_url, result)
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_back_off_without_unbounded_delay() {
        assert_eq!(retry_delay(1), Duration::from_secs(1));
        assert_eq!(retry_delay(2), Duration::from_secs(2));
        assert_eq!(retry_delay(6), Duration::from_secs(30));
        assert_eq!(retry_delay(u8::MAX), Duration::from_secs(30));
    }

    #[test]
    fn clearing_a_manual_endpoint_clears_its_parallel_backoff() {
        let state = ManualConnectorState::default();
        let url: Url = "tcp://127.0.0.1:11010".parse().unwrap();
        state.connectors.insert(url.clone());
        state.parallel_retries.insert(
            url.clone(),
            RetryState {
                failures: 3,
                next_attempt: Instant::now(),
            },
        );
        assert!(state.remove_connector(&url));
        assert!(state.parallel_retries.is_empty());
    }
}
