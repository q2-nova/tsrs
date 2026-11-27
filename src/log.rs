//! Log lines on stderr (q2nd sends them to /run/log/tailscale.log). Fixed
//! words and numbers only: never an address, name, key or login URL.
use std::sync::OnceLock;
use std::time::Instant;

pub const GENERATION_VAR: &str = "Q2_TSRSD_GENERATION";

static START: OnceLock<Instant> = OnceLock::new();

/// Restarts of this process by re-execution so far (0 for a fresh start by q2nd).
pub fn generation() -> u32 {
    static GENERATION: OnceLock<u32> = OnceLock::new();
    *GENERATION.get_or_init(|| std::env::var(GENERATION_VAR).ok().and_then(|g| g.parse().ok()).unwrap_or(0))
}

pub fn start() {
    START.get_or_init(Instant::now);
}

pub fn line(args: std::fmt::Arguments<'_>) {
    let elapsed = START.get().map(|s| s.elapsed().as_secs_f32()).unwrap_or(0.0);
    eprintln!("tsrsd g{} {elapsed:.1}s {args}", generation());
}

macro_rules! log {
    ($($t:tt)*) => { $crate::log::line(format_args!($($t)*)) };
}
pub(crate) use log;

/// Whether per-request diagnostic lines are written (`TSRS_DIAG` set; rigs only).
pub fn diag() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("TSRS_DIAG").is_some())
}
