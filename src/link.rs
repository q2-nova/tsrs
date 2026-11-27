//! The host's own link: the interface with the IPv4 default route, its
//! gateway, its addresses and its carrier. Unpatched tailscale-rs keeps its
//! DERP and control connections, and the paths it found, across a change of
//! these (no read timeout on DERP, a best path that is never withdrawn), so
//! after a Wi-Fi radio wake it reached peers only once the stall watchdog
//! restarted it. When the link returns after a loss or changes
//! ([`LinkWatch`], told by route netlink events ([`events`]) and checked again
//! on every watchdog tick), the daemon tells a running device
//! (`Device::link_changed`, tailscale-rs.patch), which reconnects in place;
//! it restarts the process if that does not finish in time ([`recovery`]),
//! or if the device was not running yet ([`decide`]).
use std::io;
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Duration;
use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc;

/// Events closer together than this are taken as one change (DHCP adds the
/// address, then the route).
pub const SETTLE: Duration = Duration::from_millis(300);
/// A burst of events is acted on after this at the latest.
pub const SETTLE_MAX: Duration = Duration::from_secs(2);

/// What identifies the link the device's connections were made on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint {
    pub iface: String,
    pub gateway: u32,
    /// The interface's IPv4 addresses, sorted.
    pub addrs: Vec<Ipv4Addr>,
}

/// The IPv4 default route with the lowest metric among those that are up: its
/// interface and gateway (as /proc/net/route writes it).
pub fn default_route(table: &str) -> Option<(String, u32)> {
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            // Iface Destination Gateway Flags RefCnt Use Metric Mask …; flag 0x1 is RTF_UP.
            if f.len() < 8 || f[1] != "00000000" || f[7] != "00000000" {
                return None;
            }
            let flags = u32::from_str_radix(f[3], 16).ok()?;
            if flags & 1 == 0 {
                return None;
            }
            Some((f[6].parse::<u32>().unwrap_or(u32::MAX), f[0].to_owned(), u32::from_str_radix(f[2], 16).ok()?))
        })
        .min_by_key(|(metric, _, _)| *metric)
        .map(|(_, iface, gateway)| (iface, gateway))
}

/// One interface address as getifaddrs(3) reports it.
#[derive(Debug, Clone)]
pub struct IfAddr {
    pub iface: String,
    /// IFF_UP and IFF_RUNNING (carrier).
    pub running: bool,
    pub v4: Option<Ipv4Addr>,
}

/// The link now, or `None` without a default route, a running interface for it
/// or an IPv4 address on that interface.
pub fn fingerprint(route_table: &str, addrs: &[IfAddr]) -> Option<Fingerprint> {
    let (iface, gateway) = default_route(route_table)?;
    let mine: Vec<&IfAddr> = addrs.iter().filter(|a| a.iface == iface).collect();
    if !mine.iter().any(|a| a.running) {
        return None;
    }
    let mut v4: Vec<Ipv4Addr> = mine.iter().filter_map(|a| a.v4).collect();
    if v4.is_empty() {
        return None;
    }
    v4.sort();
    v4.dedup();
    Some(Fingerprint { iface, gateway, addrs: v4 })
}

/// The interfaces' addresses (getifaddrs(3)); empty if it fails.
pub fn interface_addrs() -> Vec<IfAddr> {
    let mut out = Vec::new();
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs writes a list it allocated to `head`; the list is only
    // read below, while it is alive, and freed once with freeifaddrs.
    unsafe {
        if libc::getifaddrs(&mut head) != 0 {
            return out;
        }
        let mut at = head;
        while let Some(entry) = at.as_ref() {
            at = entry.ifa_next;
            if entry.ifa_name.is_null() {
                continue;
            }
            let iface = std::ffi::CStr::from_ptr(entry.ifa_name).to_string_lossy().into_owned();
            let flags = entry.ifa_flags;
            let running = flags & libc::IFF_UP as u32 != 0 && flags & libc::IFF_RUNNING as u32 != 0;
            let v4 = match entry.ifa_addr.as_ref() {
                Some(sa) if i32::from(sa.sa_family) == libc::AF_INET => {
                    let sin = &*(entry.ifa_addr as *const libc::sockaddr_in);
                    Some(Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr)))
                }
                _ => None,
            };
            out.push(IfAddr { iface, running, v4 });
        }
        libc::freeifaddrs(head);
    }
    out
}

/// The link now, read from the system.
pub fn current() -> Option<Fingerprint> {
    fingerprint(&std::fs::read_to_string("/proc/net/route").unwrap_or_default(), &interface_addrs())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// The link is back after a time without one.
    Returned,
    /// Another interface, gateway or address.
    Changed,
}

impl Change {
    pub fn word(self) -> &'static str {
        match self {
            Change::Returned => "returned",
            Change::Changed => "changed",
        }
    }
}

/// Follows the link from the process's start.
#[derive(Debug)]
pub struct LinkWatch {
    last: Option<Fingerprint>,
    lost: bool,
}

impl LinkWatch {
    /// The link at the start. A process that starts without one counts its
    /// arrival as a return.
    pub fn new(now: Option<Fingerprint>) -> Self {
        LinkWatch { lost: now.is_none(), last: now }
    }

    /// The link now; a change since the last observation that the device's
    /// connections do not survive.
    pub fn observe(&mut self, now: Option<Fingerprint>) -> Option<Change> {
        let Some(now) = now else {
            self.lost = true;
            return None;
        };
        let change = if self.lost {
            Some(Change::Returned)
        } else if self.last.as_ref() != Some(&now) {
            Some(Change::Changed)
        } else {
            None
        };
        self.lost = false;
        self.last = Some(now);
        change
    }
}

/// The device's state when the link changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceState {
    /// A login link is shown (or the auth key was rejected).
    NeedsLogin,
    /// Not running yet: starting, or registering with the stored key.
    Starting,
    Running,
}

/// What to do about a link change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Nothing,
    /// Nothing is done for it (a restart would replace the login link shown).
    Ignore(Change),
    /// Tell the running device; it reconnects in place (tailscale-rs.patch).
    Recover(Change),
    /// Restart the process: a device that is not running has nothing to keep.
    Restart(Change),
    /// Restart once the process's hold-off has passed.
    HoldOff(Duration),
}

/// Decide about `pending`, the change not yet acted on (with `observed` added), for a device in
/// `state`, in a process `age` old. A restart waits for `hold_off` from the process's start, so
/// a link that comes and goes restarts it at most that often; recovery in place has no hold-off.
pub fn decide(pending: &mut Option<Change>, observed: Option<Change>, state: DeviceState, age: Duration, hold_off: Duration) -> Step {
    if observed.is_some() {
        *pending = observed;
    }
    let Some(change) = *pending else { return Step::Nothing };
    match state {
        DeviceState::NeedsLogin => {
            *pending = None;
            Step::Ignore(change)
        }
        DeviceState::Running => {
            *pending = None;
            Step::Recover(change)
        }
        DeviceState::Starting if age < hold_off => Step::HoldOff(hold_off - age),
        DeviceState::Starting => {
            *pending = None;
            Step::Restart(change)
        }
    }
}

/// How far the device's recovery from link change `generation` has got, from the patch's
/// counters (the latest generation for which the home DERP connection, and a new control map
/// stream, came back).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovery {
    Waiting,
    /// The home DERP connection is back: peers are reachable through it.
    DerpBack,
    /// DERP and control are both back.
    Done,
    /// Not done within the limit: the process restarts instead.
    TimedOut,
}

pub fn recovery(generation: u32, derp_gen: u32, control_gen: u32, elapsed: Duration, limit: Duration) -> Recovery {
    match (derp_gen >= generation, control_gen >= generation) {
        (true, true) => Recovery::Done,
        _ if elapsed >= limit => Recovery::TimedOut,
        (true, false) => Recovery::DerpBack,
        _ => Recovery::Waiting,
    }
}

fn netlink_socket() -> io::Result<OwnedFd> {
    // SAFETY: plain socket(2) and bind(2) calls on a descriptor owned here; the
    // address is a zeroed sockaddr_nl with its family and groups set, passed
    // with its own size.
    unsafe {
        let fd = libc::socket(libc::AF_NETLINK, libc::SOCK_RAW | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC, libc::NETLINK_ROUTE);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = OwnedFd::from_raw_fd(fd);
        let mut sa: libc::sockaddr_nl = std::mem::zeroed();
        sa.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        sa.nl_groups = (libc::RTMGRP_LINK | libc::RTMGRP_IPV4_IFADDR | libc::RTMGRP_IPV4_ROUTE) as u32;
        let len = std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t;
        if libc::bind(fd.as_raw_fd(), (&sa as *const libc::sockaddr_nl).cast(), len) != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(fd)
    }
}

/// Read and drop what the socket holds; whether anything (or an overrun) was there.
fn drain(fd: &OwnedFd) -> io::Result<bool> {
    let mut buf = [0u8; 8192];
    let mut any = false;
    loop {
        // SAFETY: recv(2) into a stack buffer of the length given.
        let n = unsafe { libc::recv(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), libc::MSG_DONTWAIT) };
        if n > 0 {
            any = true;
            continue;
        }
        if n == 0 {
            return Ok(any);
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::EAGAIN) => return Ok(any),
            Some(libc::EINTR) => continue,
            // Events were lost: the link may have changed.
            Some(libc::ENOBUFS) => any = true,
            _ => return Err(e),
        }
    }
}

/// What [`events`] reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// The first link, address or route event of a burst.
    Burst,
    /// The burst has settled: time to look at the link.
    Settled,
}

/// Report bursts of link, address and route events. Returns if route netlink is
/// not available; the watchdog tick still checks the link.
pub async fn events(tx: mpsc::UnboundedSender<Event>) {
    // SAFETY: the AsyncFd owns the OwnedFd, which stays open and is the same descriptor
    // until the AsyncFd is dropped.
    let registered = netlink_socket().and_then(|fd| unsafe { AsyncFd::register_with_interest(fd, tokio::io::Interest::READABLE) }.map_err(io::Error::from));
    let fd = match registered {
        Ok(fd) => fd,
        Err(e) => {
            crate::log::log!("link_events_unavailable kind={:?}", e.kind());
            return;
        }
    };
    loop {
        let mut first = true;
        let burst = tokio::time::Instant::now();
        loop {
            // The first wait has no limit; later ones end the burst after SETTLE of quiet.
            let wait = async {
                let mut guard = fd.readable().await?;
                let got = drain(fd.get_ref());
                guard.clear_ready();
                got
            };
            let got = if first {
                Some(wait.await)
            } else {
                let left = SETTLE_MAX.saturating_sub(burst.elapsed());
                tokio::time::timeout(SETTLE.min(left), wait).await.ok()
            };
            match got {
                Some(Ok(true)) => {
                    if first && tx.send(Event::Burst).is_err() {
                        return;
                    }
                    first = false;
                }
                Some(Ok(false)) => {}
                Some(Err(e)) => {
                    crate::log::log!("link_events_error kind={:?}", e.kind());
                    return;
                }
                None => break,
            }
        }
        if tx.send(Event::Settled).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n";

    fn table(rows: &[&str]) -> String {
        format!("{HEAD}{}", rows.iter().map(|r| format!("{r}\n")).collect::<String>())
    }

    fn addr(iface: &str, running: bool, v4: Option<&str>) -> IfAddr {
        IfAddr { iface: iface.into(), running, v4: v4.map(|a| a.parse().unwrap()) }
    }

    #[test]
    fn default_route_by_metric_and_flags() {
        let net = "wlan0\t0001A8C0\t00000000\t0001\t0\t0\t0\t00FFFFFF\t0\t0\t0";
        assert_eq!(default_route(&table(&[net])), None);
        let wlan = "wlan0\t00000000\t0101A8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0";
        let eth = "eth0\t00000000\t014D590A\t0003\t0\t0\t100\t00000000\t0\t0\t0";
        assert_eq!(default_route(&table(&[net, wlan])), Some(("wlan0".into(), 0x0101A8C0)));
        assert_eq!(default_route(&table(&[wlan, eth])), Some(("eth0".into(), 0x014D590A)));
        let down = "eth0\t00000000\t014D590A\t0002\t0\t0\t100\t00000000\t0\t0\t0";
        assert_eq!(default_route(&table(&[down])), None, "not up");
        assert_eq!(default_route(""), None);
    }

    #[test]
    fn fingerprint_needs_route_carrier_and_address() {
        let t = table(&["wlan0\t00000000\t0101A8C0\t0003\t0\t0\t0\t00000000\t0\t0\t0"]);
        let up = [addr("lo", true, Some("127.0.0.1")), addr("wlan0", true, None), addr("wlan0", true, Some("192.168.1.20"))];
        let f = fingerprint(&t, &up).unwrap();
        assert_eq!((f.iface.as_str(), f.addrs.len()), ("wlan0", 1));
        assert_eq!(fingerprint(&table(&[]), &up), None, "no route");
        assert_eq!(fingerprint(&t, &[addr("wlan0", false, Some("192.168.1.20"))]), None, "no carrier");
        assert_eq!(fingerprint(&t, &[addr("wlan0", true, None)]), None, "no address");
        assert_eq!(fingerprint(&t, &[addr("eth0", true, Some("10.0.0.2"))]), None, "address on another interface");
    }

    fn fp(gateway: u32, a: &str) -> Option<Fingerprint> {
        Some(Fingerprint { iface: "wlan0".into(), gateway, addrs: vec![a.parse().unwrap()] })
    }

    #[test]
    fn a_return_after_a_loss_or_a_new_address_is_a_change() {
        let mut w = LinkWatch::new(fp(1, "192.168.1.20"));
        assert_eq!(w.observe(fp(1, "192.168.1.20")), None, "unchanged");
        // Radio down: route and address go.
        assert_eq!(w.observe(None), None);
        assert_eq!(w.observe(None), None);
        // Back with the same lease: the old connections are stale all the same.
        assert_eq!(w.observe(fp(1, "192.168.1.20")), Some(Change::Returned));
        assert_eq!(w.observe(fp(1, "192.168.1.20")), None, "once");
        // Another address or gateway without a loss in between (roaming, a new lease).
        assert_eq!(w.observe(fp(1, "192.168.1.21")), Some(Change::Changed));
        assert_eq!(w.observe(fp(2, "192.168.1.21")), Some(Change::Changed));
        assert_eq!(w.observe(fp(2, "192.168.1.21")), None);
    }

    #[test]
    fn a_start_without_a_link_counts_its_arrival() {
        let mut w = LinkWatch::new(None);
        assert_eq!(w.observe(None), None);
        assert_eq!(w.observe(fp(1, "10.0.0.2")), Some(Change::Returned));
        assert_eq!(w.observe(fp(1, "10.0.0.2")), None);
    }

    #[test]
    fn a_running_device_recovers_in_place_and_a_starting_one_restarts() {
        let (s, hold) = (Duration::from_secs, Duration::from_secs(10));
        let mut pending = None;
        assert_eq!(decide(&mut pending, None, DeviceState::Running, s(60), hold), Step::Nothing);
        // Running: in place at once, even in the process's first seconds.
        assert_eq!(decide(&mut pending, Some(Change::Returned), DeviceState::Running, s(1), hold), Step::Recover(Change::Returned));
        assert_eq!(pending, None);
        // A login link is shown: nothing.
        assert_eq!(decide(&mut pending, Some(Change::Changed), DeviceState::NeedsLogin, s(60), hold), Step::Ignore(Change::Changed));
        assert_eq!(pending, None);
        // Starting: a restart, after the hold-off; the change waits for it.
        assert_eq!(decide(&mut pending, Some(Change::Returned), DeviceState::Starting, s(4), hold), Step::HoldOff(s(6)));
        assert_eq!(decide(&mut pending, None, DeviceState::Starting, s(10), hold), Step::Restart(Change::Returned));
        assert_eq!(decide(&mut pending, None, DeviceState::Starting, s(11), hold), Step::Nothing);
        // A change that waited for the hold-off is recovered in place if the device got running meanwhile.
        decide(&mut pending, Some(Change::Changed), DeviceState::Starting, s(2), hold);
        assert_eq!(decide(&mut pending, None, DeviceState::Running, s(5), hold), Step::Recover(Change::Changed));
    }

    #[test]
    fn recovery_needs_derp_and_control_of_this_generation() {
        let (ms, limit) = (Duration::from_millis, Duration::from_secs(8));
        assert_eq!(recovery(3, 2, 2, ms(100), limit), Recovery::Waiting, "earlier generations do not count");
        assert_eq!(recovery(3, 3, 2, ms(900), limit), Recovery::DerpBack);
        assert_eq!(recovery(3, 2, 3, ms(900), limit), Recovery::Waiting, "control alone does not reach peers");
        assert_eq!(recovery(3, 3, 3, ms(1500), limit), Recovery::Done);
        assert_eq!(recovery(3, 4, 4, ms(1500), limit), Recovery::Done, "a later generation covers this one");
        assert_eq!(recovery(3, 3, 2, ms(8000), limit), Recovery::TimedOut);
        assert_eq!(recovery(3, 3, 3, ms(9000), limit), Recovery::Done, "done wins over late");
    }

    #[test]
    fn the_hosts_link_reads() {
        // Whatever the host has: the call works and agrees with itself.
        let addrs = interface_addrs();
        assert!(addrs.iter().any(|a| a.iface == "lo"), "loopback listed");
        let _ = current();
    }
}
