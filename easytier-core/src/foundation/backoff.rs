/// Saturating backoff sequence shared by the P2P retry loops (direct
/// connector, hole-punch engines). `next_backoff` walks the configured
/// delays and stays on the last one; `rollback` recovers one step when an
/// attempt partially succeeded.
#[derive(Debug)]
pub struct BackOff {
    backoffs_ms: Vec<u64>,
    current_idx: usize,
}

impl BackOff {
    pub fn new(backoffs_ms: Vec<u64>) -> Self {
        Self {
            backoffs_ms,
            current_idx: 0,
        }
    }

    pub fn next_backoff(&mut self) -> u64 {
        if self.backoffs_ms.is_empty() {
            return 0;
        }
        let backoff = self.backoffs_ms[self.current_idx];
        self.current_idx = (self.current_idx + 1).min(self.backoffs_ms.len() - 1);
        backoff
    }

    /// Next delay with symmetric jitter of half the base, matching the
    /// direct connector's per-URL retry spread.
    pub fn next_backoff_jittered(&mut self) -> u64 {
        use rand::Rng as _;
        let base = self.next_backoff() as i64;
        // `gen_range(-0..0)` would panic on an empty range, and a single
        // millisecond of base has no room for symmetric jitter anyway.
        if base == 0 {
            return 0;
        }
        let delta = (base >> 1).max(1);
        (base + rand::thread_rng().gen_range(-delta..delta)) as u64
    }

    pub fn rollback(&mut self) {
        self.current_idx = self.current_idx.saturating_sub(1);
    }

    pub async fn sleep_for_next_backoff(&mut self) {
        let backoff = self.next_backoff();
        if backoff > 0 {
            crate::foundation::time::sleep(crate::foundation::time::Duration::from_millis(backoff))
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_saturates_and_can_rollback() {
        let mut backoff = BackOff::new(vec![10, 20]);

        assert_eq!(backoff.next_backoff(), 10);
        assert_eq!(backoff.next_backoff(), 20);
        assert_eq!(backoff.next_backoff(), 20);
        backoff.rollback();
        assert_eq!(backoff.next_backoff(), 10);
    }

    #[test]
    fn jittered_backoff_stays_within_half_base() {
        let mut backoff = BackOff::new(vec![1000]);
        for _ in 0..64 {
            let delay = backoff.next_backoff_jittered() as i64;
            assert!((500..1500).contains(&delay));
        }
    }

    #[test]
    fn degenerate_backoff_sequences_do_not_panic() {
        // An empty sequence or a zero delay must degrade to "no wait"
        // instead of panicking on an out-of-bounds index or an empty
        // `gen_range` sample.
        let mut backoff = BackOff::new(vec![]);
        assert_eq!(backoff.next_backoff(), 0);
        assert_eq!(backoff.next_backoff_jittered(), 0);

        let mut zero = BackOff::new(vec![0]);
        assert_eq!(zero.next_backoff_jittered(), 0);
    }
}
