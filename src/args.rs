//! Command lines. As a daemon, tsrsd accepts what q2nd passes tailscaled
//! (`--socket=`, `--statedir=`, `--outbound-http-proxy-listen=`; `--tun=`,
//! `--socks5-server=`, `--state=`, `--port=` accepted and ignored). With a
//! subcommand it is the CLI shim (`up`, `status`, `logout`, `version`), so
//! one file serves as both `tailscaled` and `tailscale`.
use std::net::SocketAddr;
use std::path::PathBuf;

pub const DEFAULT_SOCKET: &str = "/tmp/q2nd-tailscale.sock";
pub const DEFAULT_STATEDIR: &str = "/state/network/tailscale-rs";
pub const DEFAULT_PROXY: &str = "127.0.0.1:1055";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Daemon {
    pub socket: PathBuf,
    /// Where keys.json lives.
    pub key_dir: PathBuf,
    pub proxy: SocketAddr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Up,
    Status { json: bool },
    Logout,
    Version,
    /// `diag HOST:PORT`: one proxy connection attempt, reported in fixed words.
    Diag { target: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cli {
    pub socket: PathBuf,
    pub command: Command,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Daemon(Daemon),
    Cli(Cli),
}

/// The key directory for a `--statedir`: tailscaled's own directory
/// (`…/tailscale`, as q2nd passes it to the Go daemon) maps to its sibling
/// `…/tailscale-rs`, so that tailscaled.state is never beside our keys.
pub fn key_dir(statedir: PathBuf) -> PathBuf {
    if statedir.file_name().is_some_and(|n| n == "tailscale") { statedir.with_file_name("tailscale-rs") } else { statedir }
}

pub fn parse(args: &[String]) -> Result<Mode, String> {
    let mut socket = PathBuf::from(DEFAULT_SOCKET);
    let mut statedir = PathBuf::from(DEFAULT_STATEDIR);
    let mut proxy: SocketAddr = DEFAULT_PROXY.parse().expect("default proxy address");
    let mut command = None;
    let mut json = false;
    for arg in args {
        if command.is_none() && !arg.starts_with('-') {
            command = Some(match arg.as_str() {
                "up" | "login" => Command::Up,
                "status" => Command::Status { json: false },
                "logout" => Command::Logout,
                "version" => Command::Version,
                "diag" => Command::Diag { target: String::new() },
                other => return Err(format!("unknown command {other:?}")),
            });
            continue;
        }
        if let Some(Command::Diag { target }) = &mut command {
            if target.is_empty() && !arg.starts_with('-') {
                *target = arg.clone();
                continue;
            }
        }
        let (name, value) = arg.split_once('=').unwrap_or((arg.as_str(), ""));
        match (name, command.is_some()) {
            ("--socket", _) => socket = PathBuf::from(value),
            ("--json", true) => json = true,
            // The CLI's other flags (`up --hostname=q2 --accept-dns=false --timeout=5s`) change nothing here.
            (_, true) => {}
            ("--statedir", false) => statedir = PathBuf::from(value),
            ("--outbound-http-proxy-listen", false) => proxy = value.parse().map_err(|_| format!("bad {name}"))?,
            ("--tun" | "--socks5-server" | "--state" | "--port" | "--verbose" | "--no-logs-no-support", false) => {}
            (_, false) => return Err(format!("unknown flag {name}")),
        }
        if value.is_empty() && matches!(name, "--socket" | "--statedir") {
            return Err(format!("{name} needs a value"));
        }
    }
    Ok(match command {
        Some(Command::Status { .. }) => Mode::Cli(Cli { socket, command: Command::Status { json } }),
        Some(Command::Diag { target }) if target.is_empty() => return Err("diag needs HOST:PORT".into()),
        Some(command) => Mode::Cli(Cli { socket, command }),
        None => Mode::Daemon(Daemon { socket, key_dir: key_dir(statedir), proxy }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn q2nds_tailscaled_command_line_is_accepted() {
        let line = "--tun=userspace-networking --outbound-http-proxy-listen=127.0.0.1:1055 --socks5-server=127.0.0.1:1055 \
                    --socket=/tmp/q2nd-tailscale.sock --statedir=/state/network/tailscale --state=/state/network/tailscale/tailscaled.state";
        assert_eq!(parse(&args(line)), Ok(Mode::Daemon(Daemon {
            socket: "/tmp/q2nd-tailscale.sock".into(),
            key_dir: "/state/network/tailscale-rs".into(),
            proxy: "127.0.0.1:1055".parse().unwrap(),
        })));
        let Ok(Mode::Daemon(d)) = parse(&args("--statedir=/x/rs --outbound-http-proxy-listen=127.0.0.1:2000")) else { panic!() };
        assert_eq!((d.key_dir, d.proxy.port()), ("/x/rs".into(), 2000));
        assert!(parse(&args("--bogus=1")).is_err());
        assert!(parse(&args("--socket=")).is_err());
    }

    #[test]
    fn q2nds_cli_command_lines_are_the_shim() {
        assert_eq!(parse(&args("--socket=/s up --hostname=q2 --accept-dns=false --timeout=5s")),
                   Ok(Mode::Cli(Cli { socket: "/s".into(), command: Command::Up })));
        assert_eq!(parse(&args("--socket=/s status --json")), Ok(Mode::Cli(Cli { socket: "/s".into(), command: Command::Status { json: true } })));
        assert_eq!(parse(&args("logout")), Ok(Mode::Cli(Cli { socket: DEFAULT_SOCKET.into(), command: Command::Logout })));
        assert!(parse(&args("ping x")).is_err());
        assert_eq!(parse(&args("--socket=/s diag nas:4533")),
                   Ok(Mode::Cli(Cli { socket: "/s".into(), command: Command::Diag { target: "nas:4533".into() } })));
        assert!(parse(&args("diag")).is_err());
    }
}
