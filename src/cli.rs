//! The CLI shim: the `tailscale` commands q2nd and the rigs run, against the
//! daemon's LocalAPI socket. `up` asks for a fresh login link if the current
//! one is stale (the daemon signs in on its own); `logout` forgets this node.
use crate::args::{Cli, Command};
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

/// One LocalAPI exchange: (status code, body).
pub fn request(socket: &Path, method: &str, path: &str) -> io::Result<(u16, String)> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(if path.contains("q2-diag") { 30 } else { 5 })))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    stream.write_all(format!("{method} {path} HTTP/1.0\r\nHost: local-tailscaled.sock\r\nContent-Length: 0\r\n\r\n").as_bytes())?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    let text = String::from_utf8_lossy(&response);
    let (head, body) = text.split_once("\r\n\r\n").ok_or_else(|| io::Error::other("no response"))?;
    let code = head.split(' ').nth(1).and_then(|c| c.parse().ok()).ok_or_else(|| io::Error::other("no status"))?;
    Ok((code, body.to_owned()))
}

pub fn run(cli: Cli) -> ExitCode {
    let diag;
    let (method, path) = match &cli.command {
        Command::Version => {
            println!("tsrsd {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Command::Diag { target } => {
            diag = format!("/localapi/v0/q2-diag?target={target}");
            ("GET", diag.as_str())
        }
        Command::Status { .. } => ("GET", "/localapi/v0/status"),
        Command::Up => ("POST", "/localapi/v0/login-interactive"),
        Command::Logout => ("POST", "/localapi/v0/logout"),
    };
    match request(&cli.socket, method, path) {
        Ok((200, body)) => {
            match cli.command {
                Command::Status { json: true } | Command::Diag { .. } => println!("{body}"),
                Command::Status { json: false } => {
                    let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
                    println!("{}", v["BackendState"].as_str().unwrap_or("Unknown"));
                }
                _ => {}
            }
            ExitCode::SUCCESS
        }
        Ok((code, _)) => {
            eprintln!("tsrsd: daemon answered {code}");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("tsrsd: daemon not reachable ({:?})", e.kind());
            ExitCode::FAILURE
        }
    }
}
