//! tsrsd: a Tailscale daemon on tailscale-rs, standing where q2nd starts the
//! Go tailscaled. It serves the HTTP proxy
//! q2nd's clients use and the LocalAPI status q2nd polls, and signs in by the
//! login link q2nd shows as a QR code.
mod args;
mod cli;
mod daemon;
mod http1;
mod keyfile;
mod link;
mod localapi;
mod log;
mod proxy;
mod sys;
mod tailnet;
mod watchdog;

use std::process::ExitCode;

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match args::parse(&argv) {
        Ok(args::Mode::Cli(cli)) => cli::run(cli),
        Ok(args::Mode::Daemon(daemon)) => {
            // Files the daemon or tailscale-rs creates are private (upstream #449).
            sys::private_umask();
            std::panic::set_hook(Box::new(|info| {
                // The location only: a panic payload can carry an address.
                let at = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_default();
                log::log!("panic at={at}");
            }));
            let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(runtime) => runtime,
                Err(_) => return ExitCode::from(1),
            };
            runtime.block_on(daemon::run(daemon))
        }
        Err(message) => {
            eprintln!("tsrsd: {message}");
            ExitCode::from(2)
        }
    }
}
