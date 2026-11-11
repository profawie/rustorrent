//! Token-bucket rate limiting for download and upload speed caps.
//!
//! Downloads are throttled by only *requesting* as many blocks as the budget allows, and
//! uploads by only *sending* as many blocks as it allows. Neither ever blocks a thread.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;

pub struct Limiter {
    /// Bytes per second; 0 means unlimited.
    rate: AtomicU64,
    state: Mutex<(f64, Instant)>,
}

impl Default for Limiter {
    fn default() -> Self {
        Limiter::new(0)
    }
}

impl Limiter {
    pub fn new(rate: u64) -> Limiter {
        Limiter {
            rate: AtomicU64::new(rate),
            state: Mutex::new((0.0, Instant::now())),
        }
    }

    pub fn rate(&self) -> u64 {
        self.rate.load(Relaxed)
    }

    pub fn set_rate(&self, rate: u64) {
        self.rate.store(rate, Relaxed);
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        *st = (0.0, Instant::now());
    }

    /// Refill and report whether there is budget left.
    fn has_budget(&self) -> bool {
        let rate = self.rate();
        if rate == 0 {
            return true;
        }
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let elapsed = now.duration_since(st.1).as_secs_f64();
        st.1 = now;
        // Allow at most one second of burst (but at least two blocks so slow limits still work).
        let cap = (rate as f64).max(32768.0);
        st.0 = (st.0 + elapsed * rate as f64).min(cap);
        st.0 > 0.0
    }

    fn spend(&self, n: u64) {
        if self.rate() == 0 {
            return;
        }
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.0 -= n as f64;
    }
}

/// Take `n` bytes from every limiter if all of them have budget left. The bucket may go
/// into debt by one block, which is repaid before anything else is allowed.
pub fn take(limiters: &[&Limiter], n: u64) -> bool {
    if limiters.iter().all(|l| l.has_budget()) {
        for l in limiters {
            l.spend(n);
        }
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn unlimited_and_limited() {
        let free = Limiter::new(0);
        for _ in 0..1000 {
            assert!(take(&[&free], 1 << 20));
        }
        let l = Limiter::new(100_000);
        let start = Instant::now();
        let mut sent = 0u64;
        while start.elapsed() < Duration::from_millis(500) {
            if take(&[&free, &l], 16384) {
                sent += 16384;
            } else {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        // ~50 KB in half a second, plus at most one block of debt.
        assert!(sent <= 50_000 + 2 * 16384, "sent {}", sent);
        assert!(sent >= 30_000, "sent {}", sent);
    }
}
