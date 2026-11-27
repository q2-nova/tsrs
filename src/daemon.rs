//! The daemon: keys, the LocalAPI socket, the proxy, the device and the
//! watchdog. A link change (the host's link returning or changing) is
//! recovered in place by a running device, and so is the loss of the control
//! server (the device reconnects with backoff). A deliberate restart (logout, a
//! stale login link, the watchdog, a link change the device did not recover
//! from in time or met before it was running, a device whose runtime stopped)
//! re-executes this binary in place:
//! same process ID, so q2nd's supervisor sees no exit, and nothing of the old
//! device survives. The proxy's listening socket is handed to the new process,
//! so a client that connects meanwhile waits in its queue instead of being
//! refused. A panic aborts the process (release profile) and q2nd restarts it.
use crate::args::Daemon;
use crate::http1;
use crate::keyfile::{self, Origin};
use crate::link::{self, LinkWatch};
use crate::localapi::{self, Backend, Request};
use crate::log::{self, log};
use crate::proxy;
use crate::tailnet::{self, Control, LinkReport, Shared, Tailnet};
use crate::watchdog::{self, Sample, Watchdog};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::process::CommandExt;
use std::process::ExitCode;
use std::sync::atomic::{AtomicI32, AtomicU32};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpSocket, UnixListener, UnixStream};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

/// A login link older than this is fetched anew when Sign In is pressed.
const LOGIN_REFRESH: Duration = Duration::from_secs(600);
/// Health text once the control server has not answered for this long.
const STARTING_HEALTH: Duration = Duration::from_secs(20);
/// The counters line, if anything changed.
const STATS_EVERY: Duration = Duration::from_secs(600);
/// No restart for a link change in a process's first this long: a link that
/// comes and goes restarts the device at most this often.
const LINK_HOLD_OFF: Duration = Duration::from_secs(10);
/// An in-place recovery from a link change (home DERP and control connections back) that has
/// not finished in this long restarts the process instead.
const RECOVERY_LIMIT: Duration = Duration::from_secs(8);
/// After a link change the proxy accepts clients once the home DERP connection is back, or
/// after this at the latest; until then they wait in the listener's queue.
const PROXY_HOLD: Duration = Duration::from_secs(3);
const RECOVERY_POLL: Duration = Duration::from_millis(50);
/// The proxy's listening socket, handed to the next process across a re-execution.
const PROXY_FD_VAR: &str = "Q2_TSRSD_PROXY_FD";
static PROXY_FD: AtomicI32 = AtomicI32::new(-1);
pub const CLIENT_NAME: &str = "q2nd-tsrsd";
pub const DEFAULT_HOSTNAME: &str = "q2";

/// Replace this process with a fresh copy of itself.
pub fn reexec(reason: &str, stall_restart_at: Option<Duration>) -> ! {
    let generation = log::generation() + 1;
    log!("restart reason={reason} next=g{generation}");
    // Rigs that start tsrsd through a loader or QEMU name it here
    // (`TSRSD_EXEC_PREFIX="/qemu -L /sysroot"`); on the Q2 it is unset.
    let prefix: Vec<String> = std::env::var("TSRSD_EXEC_PREFIX").unwrap_or_default().split_whitespace().map(str::to_owned).collect();
    // Through a loader /proc/self/exe is the loader, so the rig's argv[0] names the binary.
    let exe = match (prefix.is_empty(), std::env::args_os().next()) {
        (false, Some(argv0)) => Ok(std::path::PathBuf::from(argv0)),
        _ => std::env::current_exe(),
    };
    let error = match exe {
        Ok(exe) => {
            let mut command = match prefix.split_first() {
                Some((program, rest)) => {
                    let mut c = std::process::Command::new(program);
                    c.args(rest).arg(exe);
                    c
                }
                None => std::process::Command::new(exe),
            };
            command.args(std::env::args_os().skip(1)).env(log::GENERATION_VAR, generation.to_string());
            let fd = PROXY_FD.load(Relaxed);
            // SAFETY: fcntl(2) on the proxy listener's descriptor, which this process keeps
            // open until the exec: it is no longer closed on exec, so the next process has it.
            if fd >= 0 && unsafe { libc::fcntl(fd, libc::F_SETFD, 0) } == 0 {
                command.env(PROXY_FD_VAR, fd.to_string());
            } else {
                command.env_remove(PROXY_FD_VAR);
            }
            if let Some(at) = stall_restart_at {
                command.env(watchdog::STALL_RESTART_VAR, at.as_secs().to_string());
            }
            command.exec()
        }
        Err(e) => e,
    };
    log!("restart_failed kind={:?}", error.kind());
    // q2nd restarts an exited daemon.
    std::process::exit(75)
}

fn stats_line(shared: &Shared, counters: &proxy::Counters) -> String {
    let t = &shared.counters;
    format!(
        "stats requests={} tunnels={} reused={} failed={} stalls={} tailnet_ok={} refused={} timeouts={} tailnet_errors={} direct_ok={} direct_errors={} map_messages={} {} {}",
        counters.requests.load(Relaxed), counters.tunnels.load(Relaxed), counters.reused.load(Relaxed), counters.failed.load(Relaxed),
        counters.stalls.load(Relaxed), t.tailnet_ok.load(Relaxed), t.refused.load(Relaxed), t.timeouts.load(Relaxed),
        t.tailnet_errors.load(Relaxed), t.direct_ok.load(Relaxed), t.direct_errors.load(Relaxed),
        ts_control::q2::map_messages(), tailnet::filter_words(), link_words()
    )
}

/// The patch's link recovery counters, as fixed words.
pub fn link_words() -> String {
    use ts_control::q2::*;
    format!(
        "link_changes={} control_reconnects={} derp_connects={} derp_errors={} paths_withdrawn={} map_update_failures={} transports_closed={}",
        LINK_CHANGES.load(Relaxed), CONTROL_RECONNECTS.load(Relaxed), DERP_CONNECTS.load(Relaxed), DERP_ERRORS.load(Relaxed),
        PATHS_WITHDRAWN.load(Relaxed), MAP_UPDATE_FAILURES.load(Relaxed), ts_dataplane::async_tokio::TRANSPORTS_CLOSED.load(Relaxed)
    )
}

/// The listening socket a previous process of this lineage handed over, if it is
/// one: a TCP listener on `address`. Taken once.
fn inherited_proxy(address: std::net::SocketAddr) -> Option<TcpListener> {
    let fd: RawFd = std::env::var(PROXY_FD_VAR).ok()?.parse().ok().filter(|fd| *fd > 2)?;
    let mut listening: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: getsockopt(2) writes an int of the length given; a descriptor that is not
    // a socket fails with ENOTSOCK and is left alone.
    let ok = unsafe { libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_ACCEPTCONN, (&mut listening as *mut libc::c_int).cast(), &mut len) } == 0;
    if !ok || listening != 1 {
        return None;
    }
    // SAFETY: the descriptor is a listening socket this process owns from here on (the
    // previous process image handed it over and nothing else here refers to it).
    let std_listener = unsafe { std::net::TcpListener::from_raw_fd(fd) };
    let usable = std_listener.local_addr().is_ok_and(|a| a == address)
        && std_listener.set_nonblocking(true).is_ok()
        // SAFETY: fcntl(2) on the descriptor just taken: closed on exec again.
        && unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == 0;
    if !usable {
        return None; // dropping it closes the descriptor
    }
    TcpListener::from_std(std_listener).ok()
}

async fn bind_proxy(address: std::net::SocketAddr) -> io::Result<TcpListener> {
    if let Some(listener) = inherited_proxy(address) {
        log!("proxy_inherited");
        return Ok(listener);
    }
    let mut attempt = 0;
    loop {
        let socket = if address.is_ipv4() { TcpSocket::new_v4()? } else { TcpSocket::new_v6()? };
        socket.set_reuseaddr(true)?;
        // A predecessor stopping can hold the port for a moment.
        match socket.bind(address).and_then(|()| socket.listen(64)) {
            Ok(listener) => return Ok(listener),
            Err(e) if e.kind() == io::ErrorKind::AddrInUse && attempt < 40 => {
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

fn bind_socket(path: &std::path::Path) -> io::Result<UnixListener> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => std::fs::remove_file(path)?,
        Ok(_) => return Err(io::Error::other("LocalAPI path exists and is not a socket")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    UnixListener::bind(path)
}

async fn localapi_connection(mut stream: UnixStream, shared: &Shared, tailnet: &Tailnet) -> io::Result<()> {
    let mut buf = Vec::with_capacity(512);
    let head = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(head) = http1::parse_request(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))? {
                return Ok::<_, io::Error>(head);
            }
            if stream.read_buf(&mut buf).await? == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
        }
    })
    .await
    .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
    let (response, control) = match localapi::route(&head) {
        Ok(Request::Status) => (localapi::response(200, &localapi::status_json(&shared.status.lock().unwrap())), None),
        Ok(Request::LoginInteractive) => (localapi::response(200, "{}"), Some(Control::Login)),
        Ok(Request::Logout) => (localapi::response(200, "{}"), Some(Control::Logout)),
        Ok(Request::Diag { host, port }) => (localapi::response(200, &tailnet.diag(&host, port).await), None),
        Err(code) => (localapi::response(code, "{}"), None),
    };
    stream.write_all(&response).await?;
    stream.shutdown().await?;
    if let Some(control) = control {
        let _ = shared.control.send(control);
    }
    Ok(())
}

async fn localapi_serve(listener: UnixListener, shared: Arc<Shared>, tailnet: Arc<Tailnet>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let (shared, tailnet) = (shared.clone(), tailnet.clone());
                tokio::spawn(async move {
                    let _ = localapi_connection(stream, &shared, &tailnet).await;
                });
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}

/// A link change the running device is recovering from in place.
struct LinkRecovery {
    seq: u32,
    task: tokio::task::JoinHandle<()>,
    /// The proxy waits for the home DERP connection before it accepts clients.
    proxy_held: bool,
}

/// Tell the device of a link change and follow its recovery: report when the home DERP
/// connection is back (the proxy then accepts clients again, or after [`PROXY_HOLD`] at the
/// latest), when control is back too, or that [`RECOVERY_LIMIT`] passed first.
fn recover_in_place(device: Arc<tailscale::Device>, control: mpsc::UnboundedSender<Control>, seq: u32) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let told = Instant::now();
        let generation = match tokio::time::timeout(Duration::from_secs(2), device.link_changed()).await {
            Ok(Ok(generation)) => generation,
            _ => {
                let _ = control.send(Control::Link(LinkReport::Failed { seq }));
                return;
            }
        };
        let (mut proxy_opened, mut derp_ms, mut control_ms) = (false, 0, 0);
        loop {
            let elapsed = told.elapsed();
            let ms = elapsed.as_millis() as u32;
            let (derp_gen, control_gen) = (ts_control::q2::DERP_LINK_GEN.load(Relaxed), ts_control::q2::CONTROL_LINK_GEN.load(Relaxed));
            if derp_ms == 0 && derp_gen >= generation { derp_ms = ms.max(1); }
            if control_ms == 0 && control_gen >= generation { control_ms = ms.max(1); }
            let state = link::recovery(generation, derp_gen, control_gen, elapsed, RECOVERY_LIMIT);
            if !proxy_opened && (state != link::Recovery::Waiting || elapsed >= PROXY_HOLD) {
                proxy_opened = true;
                let _ = control.send(Control::Link(LinkReport::OpenProxy { seq }));
            }
            match state {
                link::Recovery::Done => {
                    let _ = control.send(Control::Link(LinkReport::Done { seq, generation, derp_ms, control_ms }));
                    return;
                }
                link::Recovery::TimedOut => {
                    let _ = control.send(Control::Link(LinkReport::TimedOut { seq, derp: derp_ms > 0, control: control_ms > 0 }));
                    return;
                }
                _ => tokio::time::sleep(RECOVERY_POLL).await,
            }
        }
    })
}

/// Act on a change of the host's link (link.rs): a running device recovers in place, one that
/// is not running yet restarts (after [`LINK_HOLD_OFF`] from the process's start), and nothing
/// is done while a login link is shown (a restart would replace it). Returns whether a recovery
/// started.
#[allow(clippy::too_many_arguments)]
fn link_check(watch: &mut LinkWatch, pending: &mut Option<link::Change>, shared: &Shared, counters: &proxy::Counters, started: Instant,
              wake: &mpsc::UnboundedSender<link::Event>, last_stall_restart: Option<Duration>, recovering: &mut Option<LinkRecovery>) -> bool {
    let observed = watch.observe(link::current());
    let device = shared.device.borrow().clone();
    let state = match shared.status.lock().unwrap().backend {
        Backend::NeedsLogin { .. } => link::DeviceState::NeedsLogin,
        Backend::Running { .. } if device.is_some() => link::DeviceState::Running,
        _ => link::DeviceState::Starting,
    };
    match link::decide(pending, observed, state, started.elapsed(), LINK_HOLD_OFF) {
        link::Step::Nothing => false,
        link::Step::Ignore(change) => {
            log!("link change={} restart=0", change.word());
            false
        }
        link::Step::HoldOff(left) => {
            let wake = wake.clone();
            tokio::spawn(async move {
                tokio::time::sleep(left).await;
                let _ = wake.send(link::Event::Settled);
            });
            false
        }
        link::Step::Restart(change) => {
            log!("link change={} recover=restart", change.word());
            log!("{}", stats_line(shared, counters));
            reexec("link", last_stall_restart);
        }
        link::Step::Recover(change) => {
            log!("link change={} recover=in_place", change.word());
            if let Some(previous) = recovering.take() {
                previous.task.abort();
            }
            static SEQ: AtomicU32 = AtomicU32::new(0);
            let seq = SEQ.fetch_add(1, Relaxed) + 1;
            let device = device.expect("running");
            *recovering = Some(LinkRecovery { seq, task: recover_in_place(device, shared.control.clone(), seq), proxy_held: true });
            true
        }
    }
}

fn read(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

pub async fn run(args: Daemon) -> ExitCode {
    log::start();
    log!("start version={} pid={}", env!("CARGO_PKG_VERSION"), std::process::id());
    let loaded = match keyfile::load_or_create(&args.key_dir, tailscale::keys::PersistState::default) {
        Ok(loaded) => loaded,
        Err(e) => {
            log!("keyfile_error kind={:?}", e.kind());
            return ExitCode::from(1);
        }
    };
    log!("keyfile {} mode_fixed={}", match loaded.origin {
        Origin::Existing => "existing",
        Origin::Created => "created",
        Origin::ReplacedCorrupt => "replaced_corrupt",
    }, u8::from(loaded.mode_fixed));
    let mut config = tailscale::Config::from(loaded.keys);
    config.client_name = Some(CLIENT_NAME.into());
    config.requested_hostname = Some(std::env::var("TS_HOSTNAME").unwrap_or_else(|_| DEFAULT_HOSTNAME.into()));
    if let Ok(url) = std::env::var("TS_CONTROL_URL") {
        match url.parse() {
            Ok(url) => config.control_server_url = url,
            Err(_) => log!("control_url_invalid"),
        }
    }
    let auth_key = std::env::var("TS_AUTH_KEY").ok().filter(|k| !k.is_empty());

    let (control_tx, mut control_rx) = mpsc::unbounded_channel();
    let (device_tx, device_rx) = watch::channel(None);
    let shared = Arc::new(Shared {
        status: Mutex::new(localapi::Status::default()),
        needs_login_since: Mutex::new(None),
        device: device_tx,
        control: control_tx,
        counters: Default::default(),
    });
    let socket = match bind_socket(&args.socket) {
        Ok(socket) => socket,
        Err(e) => {
            log!("socket_error kind={:?}", e.kind());
            return ExitCode::from(1);
        }
    };
    let listener = match bind_proxy(args.proxy).await {
        Ok(listener) => listener,
        Err(e) => {
            log!("proxy_error kind={:?}", e.kind());
            return ExitCode::from(1);
        }
    };
    PROXY_FD.store(listener.as_raw_fd(), Relaxed);
    log!("listening");
    #[cfg(feature = "pgo-peer")]
    if let Some((port, target)) = std::env::var(crate::peer::FORWARD_VAR).ok().as_deref().and_then(crate::peer::parse) {
        tokio::spawn(crate::peer::forward(device_rx.clone(), port, target));
    }
    let tailnet = Arc::new(Tailnet { shared: shared.clone(), device: device_rx });
    tokio::spawn(localapi_serve(socket, shared.clone(), tailnet.clone()));
    let counters = Arc::new(proxy::Counters::default());
    // Closed while a burst of link events settles, and after a link change until the home DERP
    // connection is back (PROXY_HOLD at most): a client that connects meanwhile waits in the
    // listener's queue, for the recovered device or for the next process if it restarts.
    let (accepting, accepting_rx) = watch::channel(true);
    tokio::spawn(proxy::serve(listener, tailnet, counters.clone(), Some(accepting_rx)));
    tokio::spawn(tailnet::run(shared.clone(), config, auth_key));

    let started = Instant::now();
    let (link_tx, mut link_rx) = mpsc::unbounded_channel();
    tokio::spawn(link::events(link_tx.clone()));
    let mut link_watch = LinkWatch::new(link::current());
    let mut link_change: Option<link::Change> = None;
    let mut recovering: Option<LinkRecovery> = None;
    let mut in_burst = false;
    let gate = |in_burst: bool, recovering: &Option<LinkRecovery>| {
        accepting.send_replace(!in_burst && !recovering.as_ref().is_some_and(|r| r.proxy_held));
    };
    let mut dog = Watchdog::default();
    dog.last_stall_restart = std::env::var(watchdog::STALL_RESTART_VAR).ok().and_then(|v| v.parse().ok()).map(Duration::from_secs);
    let mut tick = tokio::time::interval(watchdog::TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_stats = (Instant::now(), String::new());
    loop {
        tokio::select! {
            control = control_rx.recv() => match control {
                Some(Control::Login) => {
                    let stale = shared.needs_login_since.lock().unwrap().is_some_and(|since| since.elapsed() >= LOGIN_REFRESH);
                    if stale && matches!(shared.status.lock().unwrap().backend, Backend::NeedsLogin { .. }) {
                        reexec("login_refresh", dog.last_stall_restart);
                    }
                }
                Some(Control::Logout) => {
                    log!("{}", stats_line(&shared, &counters));
                    match keyfile::remove(&args.key_dir) {
                        Ok(existed) => log!("logout keyfile_removed={}", u8::from(existed)),
                        Err(e) => log!("logout_error kind={:?}", e.kind()),
                    }
                    reexec("logout", dog.last_stall_restart);
                }
                Some(Control::Stall) => dog.stall(started.elapsed()),
                Some(Control::Link(report)) => {
                    let current = recovering.as_ref().map(|r| r.seq);
                    match report {
                        LinkReport::OpenProxy { seq } if current == Some(seq) => {
                            if let Some(r) = recovering.as_mut() { r.proxy_held = false; }
                        }
                        LinkReport::Done { seq, generation, derp_ms, control_ms } if current == Some(seq) => {
                            log!("link recovered generation={generation} derp_ms={derp_ms} control_ms={control_ms}");
                            recovering = None;
                        }
                        LinkReport::TimedOut { seq, derp, control } if current == Some(seq) => {
                            log!("link recovery_timeout derp={} control={}", u8::from(derp), u8::from(control));
                            log!("{}", stats_line(&shared, &counters));
                            reexec("link_timeout", dog.last_stall_restart);
                        }
                        LinkReport::Failed { seq } if current == Some(seq) => {
                            log!("link recovery_failed");
                            reexec("link_timeout", dog.last_stall_restart);
                        }
                        _ => {}
                    }
                    gate(in_burst, &recovering);
                }
                None => return ExitCode::from(1),
            },
            Some(event) = link_rx.recv() => {
                in_burst = event == link::Event::Burst;
                if !in_burst {
                    link_check(&mut link_watch, &mut link_change, &shared, &counters, started, &link_tx, dog.last_stall_restart, &mut recovering);
                }
                gate(in_burst, &recovering);
            }
            _ = tick.tick() => {
                if link_check(&mut link_watch, &mut link_change, &shared, &counters, started, &link_tx, dog.last_stall_restart, &mut recovering) {
                    gate(in_burst, &recovering);
                }
                let network_up = watchdog::default_route(&read("/proc/net/route"));
                let running = matches!(shared.status.lock().unwrap().backend, Backend::Running { .. });
                {
                    let mut status = shared.status.lock().unwrap();
                    if !matches!(status.backend, Backend::NeedsLogin { url: None }) {
                        let mut health = Vec::new();
                        if !network_up { health.push("No network connection"); }
                        if status.backend == Backend::Starting && started.elapsed() >= STARTING_HEALTH {
                            health.push("Waiting for the Tailscale control server");
                        }
                        status.health = health;
                    }
                }
                if started.elapsed() >= watchdog::HOLD_OFF && shared.device.borrow().as_ref().is_some_and(|device| !device.is_running()) {
                    // The runtime stopped (an actor failed past its restart limit): nothing in it
                    // answers again, so start afresh.
                    log!("device_stopped");
                    log!("{}", stats_line(&shared, &counters));
                    reexec("device_stopped", dog.last_stall_restart);
                }
                let sample = Sample {
                    monotonic: started.elapsed(),
                    boot: watchdog::uptime(&read("/proc/uptime")).unwrap_or(started.elapsed()),
                    map_messages: ts_control::q2::map_messages(),
                    running,
                    network_up,
                    control_reconnecting: ts_control::q2::CONTROL_RECONNECTING.load(Relaxed),
                };
                if let Some(reason) = dog.tick(sample) {
                    log!("{}", stats_line(&shared, &counters));
                    reexec(reason.word(), dog.last_stall_restart);
                }
                if last_stats.0.elapsed() >= STATS_EVERY {
                    let line = stats_line(&shared, &counters);
                    if line != last_stats.1 { log!("{line}"); }
                    last_stats = (Instant::now(), line);
                }
            }
        }
    }
}
