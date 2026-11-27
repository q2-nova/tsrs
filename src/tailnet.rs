//! The tailscale-rs device: its start, login and addresses, and where the
//! proxy's hosts go. A tailnet address or a tailnet peer's name (host name
//! or full name, standing in for MagicDNS) goes through the device; anything
//! else is dialled from the host's own network, as tailscaled's outbound
//! proxy does, so internet requests keep working with Tailscale on.
use crate::localapi::{Backend, Status};
use crate::log::{self, log};
use crate::proxy::{self, Dialed, Route};
use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tailscale::{AuthState, Config, Device};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

/// Requests from the LocalAPI and the proxy to the daemon's control loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Login,
    Logout,
    Stall,
    /// Progress of an in-place recovery from a link change (daemon.rs).
    Link(LinkReport),
}

/// What the recovery task `seq` (the daemon's count of in-place recoveries) reports; times in
/// milliseconds from the device being told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkReport {
    /// The home DERP connection is back (or the proxy has waited long enough for it).
    OpenProxy { seq: u32 },
    /// `generation` is the device's count of link changes.
    Done { seq: u32, generation: u32, derp_ms: u32, control_ms: u32 },
    TimedOut { seq: u32, derp: bool, control: bool },
    /// The device could not be told.
    Failed { seq: u32 },
}

#[derive(Debug, Default)]
pub struct Counters {
    pub tailnet_ok: AtomicU32,
    pub refused: AtomicU32,
    pub timeouts: AtomicU32,
    pub tailnet_errors: AtomicU32,
    pub direct_ok: AtomicU32,
    pub direct_errors: AtomicU32,
}

/// State shared by the daemon's tasks. The mutex is never held across an await.
pub struct Shared {
    pub status: Mutex<Status>,
    /// When this process first saw NeedsLogin, for refreshing a stale login URL.
    pub needs_login_since: Mutex<Option<Instant>>,
    pub device: watch::Sender<Option<Arc<Device>>>,
    pub control: mpsc::UnboundedSender<Control>,
    pub counters: Counters,
}

impl Shared {
    pub fn set_backend(&self, backend: Backend) {
        let mut status = self.status.lock().unwrap();
        if matches!(backend, Backend::NeedsLogin { .. }) {
            self.needs_login_since.lock().unwrap().get_or_insert_with(Instant::now);
        }
        status.backend = backend;
    }
}

/// Tailscale's address ranges: 100.64.0.0/10 and fd7a:115c:a1e0::/48.
pub fn tailnet_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.octets()[0] == 100 && v4.octets()[1] & 0xc0 == 64,
        IpAddr::V6(v6) => v6.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
    }
}

/// Whether a name can only be a tailnet one: a bare host name or a `.ts.net` name.
/// Such a name waits for the device; other names are looked up only if it is ready.
pub fn tailnet_shaped(name: &str) -> bool {
    let name = name.trim_end_matches('.');
    !name.contains('.') || name.to_ascii_lowercase().ends_with(".ts.net")
}

/// A stable word for a tailscale-rs error.
pub fn error_word(error: &tailscale::Error) -> &'static str {
    match error {
        tailscale::Error::Timeout => "timeout",
        tailscale::Error::ConnectionReset => "reset",
        tailscale::Error::KeyFileRead | tailscale::Error::KeyFileWrite => "keyfile",
        tailscale::Error::AuthKeyRejected => "auth_key_rejected",
        tailscale::Error::Internal(_) => "internal",
    }
}

/// How a host was resolved, in fixed words.
#[derive(Debug, Clone, Default)]
pub struct Resolution {
    pub host_kind: &'static str,
    /// tailnet peer by name: found, none, timeout, or "" (not asked).
    pub peer_name: &'static str,
    /// The host's resolver: tailnet, other, empty, failed, timeout, or "" (not asked).
    pub dns: &'static str,
    pub tailnet: bool,
    pub target: Vec<SocketAddr>,
}

pub struct Tailnet {
    pub shared: Arc<Shared>,
    pub device: watch::Receiver<Option<Arc<Device>>>,
}

impl Tailnet {
    /// The device once running, waiting until `deadline` at most.
    async fn device(&self, deadline: Instant) -> Option<Arc<Device>> {
        let mut device = self.device.clone();
        tokio::time::timeout_at(deadline, device.wait_for(Option::is_some)).await.ok()?.ok().and_then(|d| d.clone())
    }

    /// Where `host` goes, and how that was decided (fixed words, for the diag endpoint).
    pub async fn resolve(&self, host: &str, port: u16, deadline: Instant) -> Resolution {
        let mut r = Resolution::default();
        if let Ok(ip) = host.parse::<IpAddr>() {
            r.host_kind = if tailnet_ip(ip) { "ip_tailnet" } else { "ip_other" };
            r.target = vec![SocketAddr::new(ip, port)];
            r.tailnet = tailnet_ip(ip);
            return r;
        }
        r.host_kind = if !host.contains('.') { "name_bare" } else if tailnet_shaped(host) { "name_ts_net" } else { "name_other" };
        let device = if tailnet_shaped(host) { self.device(deadline).await } else { self.device.borrow().clone() };
        if let Some(device) = device {
            r.peer_name = match tokio::time::timeout_at(deadline, device.peer_by_name(&host.to_ascii_lowercase())).await {
                Ok(Ok(Some(peer))) => {
                    r.target = vec![SocketAddr::new(IpAddr::V4(peer.tailnet_address.ipv4.addr()), port)];
                    r.tailnet = true;
                    return Resolution { peer_name: "found", ..r };
                }
                Ok(Ok(None)) => "none",
                _ => "timeout",
            };
        }
        // A name from public DNS (or the host's resolver) can still point into the tailnet:
        // then it goes through the tailnet, as tailscaled's proxy does.
        match tokio::time::timeout_at(deadline, tokio::net::lookup_host((host, port))).await {
            Ok(Ok(found)) => {
                let found: Vec<SocketAddr> = found.collect();
                let inside: Vec<SocketAddr> = found.iter().copied().filter(|a| tailnet_ip(a.ip())).collect();
                r.dns = if found.is_empty() { "empty" } else if inside.is_empty() { "other" } else { "tailnet" };
                r.tailnet = !inside.is_empty();
                r.target = if inside.is_empty() { found } else { inside };
            }
            Ok(Err(_)) => r.dns = "failed",
            Err(_) => r.dns = "timeout",
        }
        r
    }

    async fn connect(&self, host: &str, port: u16) -> io::Result<Dialed> {
        let deadline = Instant::now() + proxy::CONNECT_LIMIT;
        let counters = &self.shared.counters;
        let resolution = self.resolve(host, port, deadline).await;
        if resolution.target.is_empty() {
            counters.direct_errors.fetch_add(1, Relaxed);
            return Err(io::Error::new(io::ErrorKind::NotFound, "name not resolved"));
        }
        if !resolution.tailnet {
            let connected = tokio::time::timeout_at(deadline, TcpStream::connect(&resolution.target[..])).await;
            if log::diag() {
                log!("diag route=direct connect={}", match &connected { Ok(Ok(_)) => "ok", Ok(Err(_)) => "error", Err(_) => "timeout" });
            }
            return match connected {
                Ok(Ok(stream)) => {
                    counters.direct_ok.fetch_add(1, Relaxed);
                    stream.set_nodelay(true)?;
                    Ok(Dialed { conn: Box::new(stream), route: Route::Direct })
                }
                Ok(Err(e)) => {
                    counters.direct_errors.fetch_add(1, Relaxed);
                    Err(e)
                }
                Err(_) => {
                    counters.direct_errors.fetch_add(1, Relaxed);
                    Err(io::ErrorKind::TimedOut.into())
                }
            };
        }
        let target = resolution.target[0];
        let Some(device) = self.device(deadline).await else {
            counters.timeouts.fetch_add(1, Relaxed);
            return Err(io::Error::new(io::ErrorKind::TimedOut, "tailnet not running"));
        };
        let started = Instant::now();
        let connected = tokio::time::timeout_at(deadline, device.tcp_connect(target)).await;
        let outcome = match &connected {
            Ok(Ok(_)) => "ok",
            // A TSMP rejection from the peer arrives as a reset (tailscale-rs.patch).
            Ok(Err(tailscale::Error::ConnectionReset)) => "refused",
            Ok(Err(_)) => "error",
            Err(_) => "timeout",
        };
        // Only a connect that had most of the budget is evidence: one that waited for the
        // device to start (after a restart) had little time left.
        if outcome == "timeout" && started.elapsed() >= proxy::CONNECT_LIMIT * 4 / 5 {
            // An online peer that does not answer within the limit is a stall (upstream #332).
            let online = matches!(tokio::time::timeout(Duration::from_secs(1), device.peer_by_tailnet_ip(target.ip())).await,
                Ok(Ok(Some(peer))) if peer.status == ts_control::NodeStatus::Online);
            if online {
                let _ = self.shared.control.send(Control::Stall);
            }
        }
        if log::diag() {
            log!("diag route=tailnet connect={outcome} connect_ms={} {} last_tsmp_reason={}", started.elapsed().as_millis(), filter_words(),
                reason_word(ts_dataplane::tsmp_reject::LAST_REASON.load(Relaxed)));
        }
        match connected {
            Ok(Ok(stream)) => {
                counters.tailnet_ok.fetch_add(1, Relaxed);
                Ok(Dialed { conn: Box::new(stream), route: Route::Tailnet })
            }
            Ok(Err(tailscale::Error::ConnectionReset)) => {
                counters.refused.fetch_add(1, Relaxed);
                Err(io::ErrorKind::ConnectionRefused.into())
            }
            Ok(Err(e)) => {
                counters.tailnet_errors.fetch_add(1, Relaxed);
                Err(io::Error::other(error_word(&e)))
            }
            Err(_) => {
                counters.timeouts.fetch_add(1, Relaxed);
                Err(io::ErrorKind::TimedOut.into())
            }
        }
    }
}

/// The word for a TSMP rejection reason byte.
pub fn reason_word(reason: u32) -> &'static str {
    match reason as u8 {
        0 => "none",
        b'A' => "acl",
        b'S' => "shields_up",
        b'F' => "ip_forwarding",
        b'W' => "host_firewall",
        _ => "other",
    }
}

impl Tailnet {
    /// One connection attempt to `host:port` as the proxy makes it, reported in fixed
    /// words and numbers only: never the host, an address or a name.
    pub async fn diag(&self, host: &str, port: u16) -> String {
        use ts_dataplane::{q2_filter, tsmp_reject};
        let started = Instant::now();
        let before = (q2_filter::counts(), tsmp_reject::REJECTS.load(Relaxed), tsmp_reject::RESETS.load(Relaxed), tsmp_reject::MAYBE_BROKEN.load(Relaxed));
        let r = self.resolve(host, port, started + proxy::CONNECT_LIMIT).await;
        let device = self.device.borrow().clone();
        let mut out = serde_json::json!({
            "host_kind": r.host_kind, "peer_name": r.peer_name, "dns": r.dns,
            "route": if r.target.is_empty() { "unresolved" } else if r.tailnet { "tailnet" } else { "direct" },
            "device": if device.is_some() { "running" } else { "not_running" },
        });
        if r.tailnet {
            if let Some(device) = &device {
                let peer = tokio::time::timeout(Duration::from_secs(2), device.peer_by_tailnet_ip(r.target[0].ip())).await;
                let peer = match peer { Ok(Ok(p)) => p, _ => None };
                out["peer_known"] = u8::from(peer.is_some()).into();
                if let Some(p) = &peer {
                    out["peer_online"] = match p.status { ts_control::NodeStatus::Online => "online", ts_control::NodeStatus::Unknown => "unknown", _ => "offline" }.into();
                    out["peer_home_derp"] = u8::from(p.derp_region.is_some()).into();
                    out["peer_endpoints"] = p.underlay_addresses.len().into();
                    out["peer_tagged"] = u8::from(!p.tags.is_empty()).into();
                }
                out["self_online"] = match device.self_node().await { Ok(n) => u8::from(n.status == ts_control::NodeStatus::Online), Err(_) => 0 }.into();
            }
        }
        let connect_started = Instant::now();
        let outcome = match self.connect(host, port).await {
            Ok(_) => "ok",
            Err(e) => match e.kind() {
                io::ErrorKind::ConnectionRefused => "refused",
                io::ErrorKind::TimedOut => "timeout",
                io::ErrorKind::NotFound => "unresolved",
                _ => "error",
            },
        };
        let after = (q2_filter::counts(), tsmp_reject::REJECTS.load(Relaxed), tsmp_reject::RESETS.load(Relaxed), tsmp_reject::MAYBE_BROKEN.load(Relaxed));
        out["connect"] = outcome.into();
        out["connect_ms"] = (connect_started.elapsed().as_millis() as u64).into();
        out["filter_accepted_delta"] = after.0.0.wrapping_sub(before.0.0).into();
        out["filter_return_only_delta"] = after.0.1.wrapping_sub(before.0.1).into();
        out["filter_dropped_delta"] = after.0.2.wrapping_sub(before.0.2).into();
        out["tsmp_rejects_delta"] = after.1.wrapping_sub(before.1).into();
        out["tsmp_resets_delta"] = after.2.wrapping_sub(before.2).into();
        out["tsmp_maybe_broken_delta"] = after.3.wrapping_sub(before.3).into();
        out["last_tsmp_reason"] = reason_word(tsmp_reject::LAST_REASON.load(Relaxed)).into();
        out["map_messages"] = ts_control::q2::map_messages().into();
        out["followup_repolls"] = ts_control::q2::FOLLOWUP_REPOLLS.load(Relaxed).into();
        out["map_update_failures"] = ts_control::q2::MAP_UPDATE_FAILURES.load(Relaxed).into();
        out["link_changes"] = ts_control::q2::LINK_CHANGES.load(Relaxed).into();
        out["control_reconnects"] = ts_control::q2::CONTROL_RECONNECTS.load(Relaxed).into();
        out["derp_connects"] = ts_control::q2::DERP_CONNECTS.load(Relaxed).into();
        out["derp_errors"] = ts_control::q2::DERP_ERRORS.load(Relaxed).into();
        out["paths_withdrawn"] = ts_control::q2::PATHS_WITHDRAWN.load(Relaxed).into();
        out["direct_paths"] = ts_control::q2::DIRECT_PATHS.load(Relaxed).into();
        out.to_string()
    }
}

impl proxy::Dialer for Tailnet {
    fn dial(&self, host: &str, port: u16) -> impl Future<Output = io::Result<Dialed>> + Send {
        self.connect(host, port)
    }
    fn stalled(&self) {
        let _ = self.shared.control.send(Control::Stall);
    }
}

/// The inbound filter and TSMP counters of tailscale-rs.patch, as fixed words.
pub fn filter_words() -> String {
    let (accepted, return_only, dropped) = ts_dataplane::q2_filter::counts();
    format!(
        "filter_accepted={accepted} filter_return_only={return_only} filter_dropped={dropped} tsmp_rejects={} tsmp_resets={}",
        ts_dataplane::tsmp_reject::REJECTS.load(Relaxed),
        ts_dataplane::tsmp_reject::RESETS.load(Relaxed)
    )
}

/// Start the device, follow its login and publish it once running.
pub async fn run(shared: Arc<Shared>, config: Config, auth_key: Option<String>) {
    let mut backoff = Duration::from_secs(2);
    let device = loop {
        match Device::new(&config, auth_key.clone()).await {
            Ok(device) => break Arc::new(device),
            Err(e) => {
                log!("device_error kind={}", error_word(&e));
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
    };
    log!("device_started");
    let mut announced = false;
    loop {
        match device.is_authorized().await {
            Ok(AuthState::Authorized) => break,
            Ok(AuthState::NotAuthorized(url)) => {
                if !announced {
                    log!("needs_login");
                    announced = true;
                }
                shared.set_backend(Backend::NeedsLogin { url: Some(url.to_string()) });
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            Err(tailscale::Error::AuthKeyRejected) => {
                log!("auth_key_rejected");
                shared.set_backend(Backend::NeedsLogin { url: None });
                shared.status.lock().unwrap().health = vec!["Auth key rejected"];
                // Nothing to do but wait for a logout or a restart.
                std::future::pending::<()>().await;
            }
            Err(e) => {
                log!("auth_error kind={}", error_word(&e));
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
    log!("authorized");
    let v4 = device.ipv4_addr().await;
    let v6 = device.ipv6_addr().await;
    let ips: Vec<IpAddr> = [v4.ok().map(IpAddr::V4), v6.ok().map(IpAddr::V6)].into_iter().flatten().collect();
    shared.set_backend(Backend::Running { ips });
    *shared.needs_login_since.lock().unwrap() = None;
    let _ = shared.device.send(Some(device));
    log!("running");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tailnet_addresses_and_names() {
        for (ip, tailnet) in [("100.64.0.1", true), ("100.127.255.255", true), ("100.128.0.1", false), ("100.63.0.1", false),
                              ("192.168.1.2", false), ("fd7a:115c:a1e0::1", true), ("fd7a:115c:a1e1::1", false)] {
            assert_eq!(tailnet_ip(ip.parse().unwrap()), tailnet, "{ip}");
        }
        assert!(tailnet_shaped("nas") && tailnet_shaped("nas.tail1234.ts.net.") && tailnet_shaped("NAS.Tail.TS.NET"));
        assert!(!tailnet_shaped("music.example.com") && !tailnet_shaped("autoeq.app"));
    }
}
