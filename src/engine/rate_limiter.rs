use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use tokio::sync::Semaphore;

// 1 permit = 1 KiB. Lets us cap total permits at u32::MAX (tokio's
// acquire_many limit) while still expressing rates up to ~4 TiB/s,
// which is well past anything real network paths produce.
pub const RATE_UNIT_BYTES: u64 = 1024;
const REFILL_INTERVAL_MS: u64 = 10;

pub struct RateLimiter {
    sem: Arc<Semaphore>,
    max_permits: usize,
    bytes_per_sec: u64,
    stop: Arc<AtomicBool>,
}

impl RateLimiter {
    pub fn new(bytes_per_sec: u64) -> Arc<Self> {
        let max_permits =
            ((bytes_per_sec / RATE_UNIT_BYTES).max(1)).min((u32::MAX / 2) as u64) as usize;
        let sem = Arc::new(Semaphore::new(0));
        let stop = Arc::new(AtomicBool::new(false));

        let sem_c = sem.clone();
        let stop_c = stop.clone();
        thread::Builder::new()
            .name("azcp-rate-refill".into())
            .spawn(move || {
                // Refill is paced by real elapsed time, not a fixed
                // per-tick increment. If the scheduler delays the
                // wake-up (CI runners under load can stretch a 10 ms
                // sleep to 100 ms+), we still credit the bucket with
                // the bandwidth that should have accrued during the
                // delay. The bucket cap (`max_permits`, one second's
                // worth) caps burstiness.
                let permits_per_sec = max_permits as f64;
                let mut last = Instant::now();
                let mut leftover: f64 = 0.0;
                while !stop_c.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(REFILL_INTERVAL_MS));
                    let now = Instant::now();
                    let elapsed = now.duration_since(last).as_secs_f64();
                    last = now;
                    let mint = elapsed * permits_per_sec + leftover;
                    let mint_whole = mint.floor();
                    leftover = mint - mint_whole;
                    let cur = sem_c.available_permits();
                    if cur < max_permits {
                        let to_add =
                            std::cmp::min(mint_whole as usize, max_permits - cur);
                        if to_add > 0 {
                            sem_c.add_permits(to_add);
                        }
                    }
                }
            })
            .expect("spawn rate-limiter refill thread");

        Arc::new(Self {
            sem,
            max_permits,
            bytes_per_sec,
            stop,
        })
    }

    pub async fn acquire(&self, bytes: u64) {
        let needed = bytes
            .div_ceil(RATE_UNIT_BYTES)
            .min(self.max_permits as u64) as u32;
        match self.sem.acquire_many(needed).await {
            Ok(permit) => permit.forget(),
            Err(_) => {}
        }
    }

    pub fn bytes_per_sec(&self) -> u64 {
        self.bytes_per_sec
    }
}

impl Drop for RateLimiter {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn rate_limiter_tracks_target_within_tolerance() {
        // 10 MiB/s target, run ~3 seconds, expect ~30 MiB consumed.
        let target = 10 * 1024 * 1024;
        let limiter = RateLimiter::new(target);

        let block = 256 * 1024u64;
        let start = Instant::now();
        let mut consumed = 0u64;
        while start.elapsed() < Duration::from_secs(3) {
            limiter.acquire(block).await;
            consumed += block;
        }
        let secs = start.elapsed().as_secs_f64();
        let actual_bps = consumed as f64 / secs;
        let ratio = actual_bps / target as f64;
        assert!(
            (0.7..=1.3).contains(&ratio),
            "actual rate {actual_bps:.0} B/s vs target {target} B/s ratio={ratio:.3}"
        );
    }

    #[tokio::test]
    async fn acquire_larger_than_capacity_does_not_deadlock() {
        let limiter = RateLimiter::new(64 * 1024);
        limiter.acquire(10 * 1024 * 1024).await;
    }
}
