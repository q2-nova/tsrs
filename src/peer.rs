//! The PGO training fixture's peer (cargo feature `pgo-peer`, never in a Q2
//! build): a host-built tsrsd that also accepts tailnet connections on
//! `TSRSD_PGO_PEER_FORWARD=PORT=HOST:PORT` and forwards each to the generated
//! content server on the host. tools/pgo.py runs it beside the instrumented
//! daemon on the generated tailnet.
use crate::log::log;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use tailscale::Device;
use tokio::net::TcpStream;
use tokio::sync::watch;

pub const FORWARD_VAR: &str = "TSRSD_PGO_PEER_FORWARD";

/// `PORT=HOST:PORT`: the tailnet port and the local address it forwards to.
pub fn parse(spec: &str) -> Option<(u16, SocketAddr)> {
    let (port, target) = spec.split_once('=')?;
    Some((port.parse().ok()?, target.parse().ok()?))
}

pub async fn forward(mut device: watch::Receiver<Option<Arc<Device>>>, port: u16, target: SocketAddr) {
    let Ok(device) = device.wait_for(Option::is_some).await.map(|d| d.clone().expect("running")) else { return };
    let Ok(ip) = device.ipv4_addr().await else {
        log!("peer_forward no_address");
        return;
    };
    let listener = match device.tcp_listen(SocketAddr::new(IpAddr::V4(ip), port)).await {
        Ok(listener) => listener,
        Err(e) => {
            log!("peer_forward listen_error {e:?}");
            return;
        }
    };
    log!("peer_forward listening port={port}");
    loop {
        let Ok(mut inbound) = listener.accept().await else {
            log!("peer_forward accept_error");
            return;
        };
        tokio::spawn(async move {
            if let Ok(mut outbound) = TcpStream::connect(target).await {
                let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn forward_spec() {
        assert_eq!(super::parse("80=127.0.0.1:18080"), Some((80, "127.0.0.1:18080".parse().unwrap())));
        assert_eq!(super::parse("80"), None);
        assert_eq!(super::parse("x=127.0.0.1:1"), None);
    }
}
