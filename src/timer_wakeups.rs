//! The daemon's runtime thread wakes once per timer expiry.
//!
//! tsrsd runs one current-thread runtime (main.rs). Its timers are the
//! watchdog interval, retry and poll sleeps, and timeouts that are armed
//! around each request and dropped when it completes. Unpatched tokio 1.54.1
//! sleeps until the start of the next occupied wheel slot rather than the
//! earliest deadline in it, so every timer more than 64 ms away wakes the
//! thread about twice. The Cargo.toml patch selects the q2-nova/tokio fork,
//! which wakes once.
//!
//! A thread that sleeps in `epoll_wait` and is woken makes one voluntary
//! context switch (`/proc/thread-self/status`). The cases run in a child
//! process with one test thread, so no other test shares the counts.

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::time::Duration;

    fn voluntary_switches() -> u64 {
        let status = std::fs::read_to_string("/proc/thread-self/status").unwrap();
        status.lines().find_map(|l| l.strip_prefix("voluntary_ctxt_switches:")).unwrap().trim().parse().unwrap()
    }

    fn wakeups<F: Future<Output = u64>>(work: impl FnOnce() -> F) -> (u64, u64) {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        // Start the driver before counting.
        rt.block_on(async { tokio::time::sleep(Duration::from_millis(5)).await });
        let before = voluntary_switches();
        let expiries = rt.block_on(work());
        (expiries, voluntary_switches() - before)
    }

    /// Prints one `WAKE name expiries wakeups` line per case. Run by
    /// `timers_wake_the_runtime_thread_once_per_expiry`.
    #[test]
    #[ignore = "run by timers_wake_the_runtime_thread_once_per_expiry"]
    fn probe() {
        let report = |name: &str, (expiries, wakeups): (u64, u64)| println!("WAKE {name} {expiries} {wakeups}");
        report("sleep-250ms", wakeups(|| async {
            for _ in 0..8 { tokio::time::sleep(Duration::from_millis(250)).await; }
            8
        }));
        report("watchdog-interval", wakeups(|| async {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await;
            for _ in 0..4 { tick.tick().await; }
            4
        }));
        // 5 s timeouts armed around work that completes first, then one
        // 300 ms sleep.
        report("dropped-timeouts", wakeups(|| async {
            for _ in 0..1000 {
                let _ = tokio::time::timeout(Duration::from_secs(5), tokio::task::yield_now()).await;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
            1
        }));
    }

    /// One wakeup fires a timer; two more are allowed per case (the first
    /// park, a wake racing the end). Unpatched tokio fails the sleep and
    /// interval cases; the dropped timeouts must not wake the thread.
    #[test]
    fn timers_wake_the_runtime_thread_once_per_expiry() {
        let probe = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "timer_wakeups::tests::probe", "--ignored", "--nocapture", "--test-threads=1"])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&probe.stdout).into_owned();
        assert!(probe.status.success(), "{text}\n{}", String::from_utf8_lossy(&probe.stderr));
        let mut seen = Vec::new();
        for line in text.lines() {
            // libtest prints the test's name before its first line of output.
            let fields: Vec<&str> = line.split_whitespace().skip_while(|f| *f != "WAKE").collect();
            if let ["WAKE", name, expiries, wakeups] = fields.as_slice() {
                let (expiries, wakeups): (u64, u64) = (expiries.parse().unwrap(), wakeups.parse().unwrap());
                assert!(expiries > 0, "{name}: no timer expired");
                assert!(wakeups <= expiries + 2, "{name}: {wakeups} wakeups for {expiries} expiries");
                seen.push(name.to_string());
            }
        }
        assert_eq!(seen, ["sleep-250ms", "watchdog-interval", "dropped-timeouts"], "{text}");
    }
}
