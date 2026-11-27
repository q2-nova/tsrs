//! The part of tailscaled's LocalAPI that q2nd and the rigs use, on the same
//! Unix socket: `GET /localapi/v0/status` (BackendState, AuthURL,
//! TailscaleIPs, Health), and `POST …/login-interactive` and `POST …/logout`
//! for the CLI shim. One request per connection, answered and closed, as
//! q2nd reads the status to end of file.
use crate::http1;
use serde::Serialize;
use std::net::IpAddr;

/// The daemon's state as tailscaled names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    /// Control not yet answered.
    Starting,
    /// Not authorised; the URL to approve this node at, once control gave one.
    NeedsLogin { url: Option<String> },
    /// Authorised, with the node's addresses.
    Running { ips: Vec<IpAddr> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub backend: Backend,
    /// Fixed sentences from the daemon, never server text.
    pub health: Vec<&'static str>,
}

impl Default for Status {
    fn default() -> Self {
        Status { backend: Backend::Starting, health: Vec::new() }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct Document<'a> {
    version: &'a str,
    /// Which daemon answers: rigs tell the two apart by it.
    backend: &'a str,
    backend_state: &'a str,
    #[serde(rename = "AuthURL")]
    auth_url: &'a str,
    #[serde(rename = "TailscaleIPs")]
    tailscale_ips: Vec<String>,
    health: &'a [&'static str],
}

/// The status document, in tailscaled's field names.
pub fn status_json(status: &Status) -> String {
    let (state, url, ips) = match &status.backend {
        Backend::Starting => ("Starting", "", Vec::new()),
        Backend::NeedsLogin { url } => ("NeedsLogin", url.as_deref().unwrap_or(""), Vec::new()),
        // IPv4 first: q2nd shows the first address.
        Backend::Running { ips } => {
            let mut ips = ips.clone();
            ips.sort_by_key(|ip| ip.is_ipv6());
            ("Running", "", ips.iter().map(ToString::to_string).collect())
        }
    };
    serde_json::to_string(&Document {
        version: concat!("tsrsd ", env!("CARGO_PKG_VERSION")),
        backend: "tailscale-rs",
        backend_state: state,
        auth_url: url,
        tailscale_ips: ips,
        health: &status.health,
    })
    .expect("status serializes")
}

/// A request on the socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Status,
    LoginInteractive,
    Logout,
    /// One proxy connection attempt to `host:port`, reported in fixed words (q2 rigs).
    Diag { host: String, port: u16 },
}

/// What a request head asks for; `Err` carries the HTTP status to answer.
pub fn route(head: &http1::RequestHead) -> Result<Request, u16> {
    let (path, query) = head.target.split_once('?').unwrap_or((&head.target, ""));
    if (head.method.as_str(), path) == ("GET", "/localapi/v0/q2-diag") {
        let target = query.split('&').find_map(|kv| kv.strip_prefix("target=")).ok_or(400u16)?;
        let (host, port) = http1::split_authority(target, 443).ok_or(400u16)?;
        return Ok(Request::Diag { host, port });
    }
    match (head.method.as_str(), path) {
        ("GET", "/localapi/v0/status") => Ok(Request::Status),
        ("POST", "/localapi/v0/login-interactive") => Ok(Request::LoginInteractive),
        ("POST", "/localapi/v0/logout") => Ok(Request::Logout),
        (_, "/localapi/v0/status" | "/localapi/v0/login-interactive" | "/localapi/v0/logout") => Err(405),
        _ => Err(404),
    }
}

/// A complete response with a body; the connection closes after it.
pub fn response(code: u16, body: &str) -> Vec<u8> {
    let reason = match code {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    };
    format!("HTTP/1.0 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
        .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// The fields q2nd reads, as its parser (crates/q2nd/src/tailscale.rs parse_status) reads them.
    fn as_q2nd(json: &str) -> (String, String, String, String) {
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        (
            v["BackendState"].as_str().unwrap_or("Unknown").into(),
            v["TailscaleIPs"].as_array().and_then(|a| a.first()).and_then(|s| s.as_str()).unwrap_or("").into(),
            v["AuthURL"].as_str().unwrap_or("").into(),
            v["Health"].as_array().map(|a| a.iter().filter_map(|s| s.as_str()).collect::<Vec<_>>().join("; ")).unwrap_or_default(),
        )
    }

    #[test]
    fn documents_read_as_q2nd_reads_tailscaled() {
        let running = Status {
            backend: Backend::Running { ips: vec!["fd7a:115c:a1e0::5".parse().unwrap(), "100.64.0.5".parse().unwrap()] },
            health: vec![],
        };
        assert_eq!(as_q2nd(&status_json(&running)), ("Running".into(), "100.64.0.5".into(), "".into(), "".into()));
        let login = Status { backend: Backend::NeedsLogin { url: Some("https://login.example/a/1".into()) }, health: vec!["a", "b"] };
        assert_eq!(as_q2nd(&status_json(&login)), ("NeedsLogin".into(), "".into(), "https://login.example/a/1".into(), "a; b".into()));
        assert_eq!(as_q2nd(&status_json(&Status::default())).0, "Starting");
        let v: serde_json::Value = serde_json::from_str(&status_json(&Status::default())).unwrap();
        assert_eq!(v["Backend"], "tailscale-rs");
    }

    #[test]
    fn routes() {
        let head = |t: &str| http1::parse_request(t.as_bytes()).unwrap().unwrap();
        assert_eq!(route(&head("GET /localapi/v0/status?peers=false HTTP/1.0\r\nHost: local-tailscaled.sock\r\n\r\n")), Ok(Request::Status));
        assert_eq!(route(&head("POST /localapi/v0/logout HTTP/1.1\r\n\r\n")), Ok(Request::Logout));
        assert_eq!(route(&head("POST /localapi/v0/login-interactive HTTP/1.1\r\n\r\n")), Ok(Request::LoginInteractive));
        assert_eq!(route(&head("GET /localapi/v0/logout HTTP/1.1\r\n\r\n")), Err(405));
        assert_eq!(route(&head("GET /localapi/v0/prefs HTTP/1.1\r\n\r\n")), Err(404));
        assert_eq!(route(&head("GET /localapi/v0/q2-diag?target=nas:4533 HTTP/1.0\r\n\r\n")), Ok(Request::Diag { host: "nas".into(), port: 4533 }));
        assert_eq!(route(&head("GET /localapi/v0/q2-diag?x=1 HTTP/1.0\r\n\r\n")), Err(400));
        let r = String::from_utf8(response(200, "{}")).unwrap();
        assert!(r.starts_with("HTTP/1.0 200 OK\r\n") && r.ends_with("\r\n\r\n{}"));
    }

    proptest! {
        /// Any URL and health text round-trip through JSON exactly.
        #[test]
        fn any_login_url_round_trips(url in "\\PC{0,200}", n in 0usize..3) {
            let health: Vec<&'static str> = ["Control server unreachable", "No network", "Waiting"][..n].to_vec();
            let status = Status { backend: Backend::NeedsLogin { url: Some(url.clone()) }, health: health.clone() };
            let (state, ip, got, h) = as_q2nd(&status_json(&status));
            prop_assert_eq!((state.as_str(), ip.as_str(), got), ("NeedsLogin", "", url));
            prop_assert_eq!(h, health.join("; "));
        }
    }
}
