//! When to restart the tailscale-rs device, for faults it does not recover from:
//!
//! - control silence (upstream #210): the control connection reads no map
//!   response, keep-alives included, for [`CONTROL_SILENCE`] while the node is
//!   running, the host has a default route and the device is not already
//!   reconnecting to control (after a lost map stream it reconnects with
//!   backoff and keeps its data plane; a restart would drop that until control
//!   answers);
//! - a long suspend: the control connection is stale after the system slept
//!   longer than control's keep-alive interval, so the device restarts once the
//!   network is back rather than after a further [`CONTROL_SILENCE`];
//! - peer stalls (upstream #332, no new handshake after an unanswered
//!   keep-alive): [`STALLS`] connect timeouts to known peers, or requests with no
//!   response head after [`proxy::STALL_AFTER`](crate::proxy::STALL_AFTER), within
//!   [`STALL_WINDOW`].
//!
//! No restart in the first [`HOLD_OFF`] of a process, so a restart cannot repeat
//! faster than that.
use std::collections::VecDeque;
use std::time::Duration;

pub const CONTROL_SILENCE: Duration = Duration::from_secs(120);
pub const SLEPT: Duration = Duration::from_secs(120);
pub const STALLS: usize = 2;
pub const STALL_WINDOW: Duration = Duration::from_secs(300);
pub const HOLD_OFF: Duration = Duration::from_secs(120);
pub const TICK: Duration = Duration::from_secs(10);
/// At most one peer-stall restart in this long, across re-executions: a peer that
/// is down but still listed online would otherwise restart the daemon every few minutes.
pub const STALL_RESTART_EVERY: Duration = Duration::from_secs(900);
/// The boot-clock time of the last peer-stall restart, carried across re-execution.
pub const STALL_RESTART_VAR: &str = "Q2_TSRSD_STALL_RESTART_AT";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    ControlSilent,
    Slept,
    PeerStall,
}

impl Reason {
    pub fn word(self) -> &'static str {
        match self {
            Reason::ControlSilent => "control_silent",
            Reason::Slept => "slept",
            Reason::PeerStall => "peer_stall",
        }
    }
}

/// One observation, taken every [`TICK`]. Times are seconds on two clocks:
/// `monotonic` stops during suspend, `boot` does not.
#[derive(Debug, Clone, Copy)]
pub struct Sample {
    pub monotonic: Duration,
    pub boot: Duration,
    pub map_messages: u32,
    pub running: bool,
    pub network_up: bool,
    /// The device is opening a new control connection (tailscale-rs.patch).
    pub control_reconnecting: bool,
}

#[derive(Debug, Default)]
pub struct Watchdog {
    started: Option<Duration>,
    previous: Option<Sample>,
    /// Monotonic time of the last map response (or of the start of running).
    heard: Duration,
    slept: bool,
    stalls: VecDeque<Duration>,
    /// Boot-clock time of the last peer-stall restart (this lineage of processes).
    pub last_stall_restart: Option<Duration>,
}

impl Watchdog {
    /// A peer stall at monotonic time `at`.
    pub fn stall(&mut self, at: Duration) {
        self.stalls.push_back(at);
    }

    pub fn tick(&mut self, now: Sample) -> Option<Reason> {
        let started = *self.started.get_or_insert(now.monotonic);
        let previous = self.previous.replace(now);
        while self.stalls.front().is_some_and(|&t| now.monotonic.saturating_sub(t) > STALL_WINDOW) {
            self.stalls.pop_front();
        }
        if let Some(p) = previous {
            let asleep = now.boot.saturating_sub(p.boot).saturating_sub(now.monotonic.saturating_sub(p.monotonic));
            if asleep >= SLEPT {
                self.slept = true;
            }
            if p.map_messages != now.map_messages || !p.running {
                self.heard = now.monotonic;
            }
        } else {
            self.heard = now.monotonic;
        }
        if !now.running || !now.network_up || now.control_reconnecting {
            // Silence while not running, off the network or reconnecting says nothing new about
            // control.
            self.heard = now.monotonic;
            return None;
        }
        if now.monotonic.saturating_sub(started) < HOLD_OFF {
            // A sleep seen now is acted on when the hold-off ends.
            return None;
        }
        if self.slept {
            self.slept = false;
            return Some(Reason::Slept);
        }
        if now.monotonic.saturating_sub(self.heard) >= CONTROL_SILENCE {
            self.heard = now.monotonic;
            return Some(Reason::ControlSilent);
        }
        if self.stalls.len() >= STALLS {
            self.stalls.clear();
            if self.last_stall_restart.is_some_and(|at| now.boot.saturating_sub(at) < STALL_RESTART_EVERY) {
                return None;
            }
            self.last_stall_restart = Some(now.boot);
            return Some(Reason::PeerStall);
        }
        None
    }
}

/// Whether /proc/net/route lists an IPv4 default route.
pub fn default_route(table: &str) -> bool {
    table.lines().skip(1).any(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        // Iface Destination Gateway Flags …; flag 0x1 is RTF_UP.
        fields.len() > 3 && fields[1] == "00000000" && u32::from_str_radix(fields[3], 16).is_ok_and(|f| f & 1 != 0)
    })
}

/// Seconds since boot including suspend, from /proc/uptime.
pub fn uptime(text: &str) -> Option<Duration> {
    text.split_whitespace().next()?.parse::<f64>().ok().filter(|s| s.is_finite() && *s >= 0.0).map(Duration::from_secs_f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(at: u64, messages: u32) -> Sample {
        Sample { monotonic: Duration::from_secs(at), boot: Duration::from_secs(at), map_messages: messages, running: true, network_up: true,
                 control_reconnecting: false }
    }

    fn run(dog: &mut Watchdog, from: u64, to: u64, f: impl Fn(u64) -> Sample) -> Vec<(u64, Reason)> {
        (from..=to).step_by(10).filter_map(|t| dog.tick(f(t)).map(|r| (t, r))).collect()
    }

    #[test]
    fn control_silence_restarts_once_keepalives_stop() {
        let mut dog = Watchdog::default();
        // Keep-alives every 60 s: no restart.
        assert_eq!(run(&mut dog, 0, 600, |t| s(t, (t / 60) as u32)), vec![]);
        // They stop at 600: restart 120 s later.
        assert_eq!(run(&mut dog, 610, 800, |t| s(t, 10)), vec![(720, Reason::ControlSilent)]);
    }

    #[test]
    fn no_restart_while_not_running_off_the_network_or_in_the_hold_off() {
        let mut dog = Watchdog::default();
        assert_eq!(run(&mut dog, 0, 110, |t| s(t, 0)), vec![], "hold-off");
        assert_eq!(run(&mut dog, 120, 900, |t| Sample { running: false, ..s(t, 0) }), vec![]);
        assert_eq!(run(&mut dog, 910, 1800, |t| Sample { network_up: false, ..s(t, 0) }), vec![]);
        // Silence counts from the network's return.
        assert_eq!(run(&mut dog, 1810, 2000, |t| s(t, 0)), vec![(1920, Reason::ControlSilent)]);
    }

    #[test]
    fn no_control_silence_restart_while_the_device_reconnects() {
        let mut dog = Watchdog::default();
        assert_eq!(run(&mut dog, 0, 300, |t| s(t, (t / 60) as u32)), vec![]);
        // Control goes away at 300 and the device reconnects for 10 minutes: no restart.
        assert_eq!(run(&mut dog, 310, 900, |t| Sample { control_reconnecting: true, ..s(t, 5) }), vec![]);
        // Reconnected but silent again: silence counts from the last sample while reconnecting.
        assert_eq!(run(&mut dog, 910, 1100, |t| s(t, 5)), vec![(1020, Reason::ControlSilent)]);
    }

    #[test]
    fn a_long_suspend_restarts_when_the_network_is_back() {
        let mut dog = Watchdog::default();
        assert_eq!(run(&mut dog, 0, 300, |t| s(t, (t / 60) as u32)), vec![]);
        // Slept 10 minutes between 300 and 310 monotonic; Wi-Fi rejoins 30 s later.
        let woke = |t: u64| Sample { boot: Duration::from_secs(t + 600), network_up: t >= 340, ..s(t, 5) };
        assert_eq!(run(&mut dog, 310, 360, woke), vec![(340, Reason::Slept)]);
        // A short sleep is not one.
        let mut dog = Watchdog::default();
        run(&mut dog, 0, 300, |t| s(t, (t / 60) as u32));
        assert_eq!(dog.tick(Sample { boot: Duration::from_secs(370), ..s(310, 5) }), None);
    }

    #[test]
    fn two_stalls_in_the_window_restart() {
        let mut dog = Watchdog::default();
        run(&mut dog, 0, 200, |t| s(t, (t / 30) as u32));
        dog.stall(Duration::from_secs(205));
        assert_eq!(dog.tick(s(210, 7)), None);
        dog.stall(Duration::from_secs(600));
        assert_eq!(dog.tick(s(610, 20)), None, "the first stall left the window");
        dog.stall(Duration::from_secs(615));
        assert_eq!(dog.tick(s(620, 20)), Some(Reason::PeerStall));
        assert_eq!(dog.tick(s(630, 21)), None);
    }

    #[test]
    fn peer_stall_restarts_are_limited_across_processes() {
        // The previous process restarted for stalls at boot time 600.
        let mut dog = Watchdog { last_stall_restart: Some(Duration::from_secs(600)), ..Watchdog::default() };
        run(&mut dog, 0, 200, |t| Sample { boot: Duration::from_secs(600 + t), ..s(t, (t / 30) as u32) });
        dog.stall(Duration::from_secs(205));
        dog.stall(Duration::from_secs(206));
        assert_eq!(dog.tick(Sample { boot: Duration::from_secs(810), ..s(210, 7) }), None, "within 15 minutes of the last");
        dog.stall(Duration::from_secs(1300));
        dog.stall(Duration::from_secs(1301));
        assert_eq!(dog.tick(Sample { boot: Duration::from_secs(1910), ..s(1310, 44) }), Some(Reason::PeerStall));
    }

    #[test]
    fn a_sleep_in_the_hold_off_restarts_when_it_ends() {
        let mut dog = Watchdog::default();
        dog.tick(s(0, 0));
        assert_eq!(dog.tick(Sample { boot: Duration::from_secs(610), ..s(10, 1) }), None, "in the hold-off");
        assert_eq!(run(&mut dog, 20, 130, |t| Sample { boot: Duration::from_secs(600 + t), ..s(t, (t / 10) as u32) }),
                   vec![(120, Reason::Slept)]);
    }

    #[test]
    fn route_table_and_uptime() {
        let table = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\n\
                     wlan0\t0001A8C0\t00000000\t0001\t0\t0\t0\t00FFFFFF\n";
        assert!(!default_route(table));
        assert!(default_route(&format!("{table}wlan0\t00000000\t0101A8C0\t0003\t0\t0\t0\t00000000\n")));
        assert!(!default_route(&format!("{table}wlan0\t00000000\t0101A8C0\t0002\t0\t0\t0\t00000000\n")), "not up");
        assert_eq!(uptime("1234.56 99.0\n"), Some(Duration::from_secs_f64(1234.56)));
        assert_eq!(uptime("x"), None);
    }
}
