//! The outbound HTTP proxy q2nd's clients use (`http://127.0.0.1:1055`):
//! `CONNECT` tunnels, and absolute-form requests forwarded one after another
//! on a kept-alive client connection, reusing the origin connection while both
//! ends keep it open. Where a host goes is the [`Dialer`]'s decision.
use crate::http1::{self, Body, Target};
use std::future::Future;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

/// A connection attempt's limit, tailnet or direct.
pub const CONNECT_LIMIT: Duration = Duration::from_secs(10);
/// A tailnet request without a response head this long counts as a peer stall
/// (the request keeps waiting; the client's own timeouts apply).
#[cfg(not(test))]
pub const STALL_AFTER: Duration = Duration::from_secs(30);
#[cfg(test)]
pub const STALL_AFTER: Duration = Duration::from_millis(300);
/// A response head that has not come in this long ends the exchange with 504,
/// so a vanished origin cannot hold one of the [`MAX_CLIENTS`] slots for good.
#[cfg(not(test))]
pub const RESPONSE_HEAD_LIMIT: Duration = Duration::from_secs(300);
#[cfg(test)]
pub const RESPONSE_HEAD_LIMIT: Duration = Duration::from_millis(1500);
/// An idle client connection between requests is closed after this.
pub const IDLE: Duration = Duration::from_secs(120);
/// Client connections served at once; further ones wait to be accepted.
pub const MAX_CLIENTS: usize = 32;
const READ_CHUNK: usize = 16 * 1024;

pub trait Conn: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> Conn for T {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Tailnet,
    Direct,
}

pub struct Dialed {
    pub conn: Box<dyn Conn>,
    pub route: Route,
}

pub trait Dialer: Send + Sync + 'static {
    /// Connect to `host:port` within [`CONNECT_LIMIT`].
    fn dial(&self, host: &str, port: u16) -> impl Future<Output = io::Result<Dialed>> + Send;
    /// A tailnet request waited [`STALL_AFTER`] for its response head.
    fn stalled(&self);
}

/// Proxy outcomes for the daemon's periodic log line.
#[derive(Debug, Default)]
pub struct Counters {
    pub requests: AtomicU32,
    pub tunnels: AtomicU32,
    pub reused: AtomicU32,
    pub failed: AtomicU32,
    pub stalls: AtomicU32,
}

/// A stream with a read buffer.
struct Buffered<S> {
    inner: S,
    buf: Vec<u8>,
    pos: usize,
}

impl<S: AsyncRead + AsyncWrite + Unpin> Buffered<S> {
    fn new(inner: S) -> Self {
        Buffered { inner, buf: Vec::new(), pos: 0 }
    }
    fn data(&self) -> &[u8] {
        &self.buf[self.pos..]
    }
    fn consume(&mut self, n: usize) {
        self.pos += n;
        if self.pos == self.buf.len() {
            self.buf.clear();
            self.pos = 0;
        }
    }
    /// Read more; 0 at end of stream.
    async fn fill(&mut self) -> io::Result<usize> {
        if self.pos > 0 && self.pos * 2 >= self.buf.len() {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        if self.buf.capacity() - self.buf.len() < READ_CHUNK / 4 {
            self.buf.reserve(READ_CHUNK);
        }
        self.inner.read_buf(&mut self.buf).await
    }
    /// The next head, or `None` if the stream ended cleanly before one.
    async fn head<T>(&mut self, parse: fn(&[u8]) -> Result<Option<T>, http1::Error>) -> io::Result<Option<T>> {
        loop {
            if let Some(head) = parse(self.data()).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))? {
                return Ok(Some(head));
            }
            if self.fill().await? == 0 {
                return if self.data().is_empty() { Ok(None) } else { Err(io::ErrorKind::UnexpectedEof.into()) };
            }
        }
    }
}

/// Copy one message body from `src` to `dst`, as framed.
async fn relay_body<R, W>(src: &mut Buffered<R>, dst: &mut W, body: Body) -> io::Result<()>
where
    R: AsyncRead + AsyncWrite + Unpin,
    W: AsyncWrite + Unpin,
{
    match body {
        Body::None => {}
        Body::Length(mut left) => {
            while left > 0 {
                if src.data().is_empty() && src.fill().await? == 0 {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
                let take = src.data().len().min(usize::try_from(left).unwrap_or(usize::MAX));
                dst.write_all(&src.data()[..take]).await?;
                src.consume(take);
                left -= take as u64;
            }
        }
        Body::Chunked => {
            let mut scanner = http1::Chunked::default();
            loop {
                if src.data().is_empty() && src.fill().await? == 0 {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
                let (used, done) = scanner.scan(src.data()).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                dst.write_all(&src.data()[..used]).await?;
                src.consume(used);
                if done {
                    break;
                }
            }
        }
        Body::Close => loop {
            if !src.data().is_empty() {
                dst.write_all(src.data()).await?;
                let n = src.data().len();
                src.consume(n);
            }
            if src.fill().await? == 0 {
                break;
            }
        },
    }
    dst.flush().await
}

struct Upstream {
    host: String,
    port: u16,
    route: Route,
    conn: Buffered<Box<dyn Conn>>,
}

async fn dial<D: Dialer>(dialer: &D, host: &str, port: u16) -> io::Result<Upstream> {
    let Dialed { conn, route } = dialer.dial(host, port).await?;
    Ok(Upstream { host: host.to_owned(), port, route, conn: Buffered::new(conn) })
}

/// Accept and serve clients until the listener fails. While `gate` is false no
/// client is accepted: they wait in the listener's queue (for the next process,
/// when the daemon is about to re-execute).
pub async fn serve<D: Dialer>(listener: TcpListener, dialer: Arc<D>, counters: Arc<Counters>, mut gate: Option<watch::Receiver<bool>>) {
    let permits = Arc::new(tokio::sync::Semaphore::new(MAX_CLIENTS));
    loop {
        let Ok(permit) = permits.clone().acquire_owned().await else { return };
        if let Some(g) = gate.as_mut()
            && g.wait_for(|open| *open).await.is_err()
        {
            gate = None;
        }
        let closed = async {
            let closing = match gate.as_mut() {
                Some(g) => g.wait_for(|open| !*open).await.is_ok(),
                None => false,
            };
            if !closing {
                std::future::pending::<()>().await;
            }
        };
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            () = closed => continue,
        };
        match accepted {
            Ok((client, _)) => {
                let (dialer, counters) = (dialer.clone(), counters.clone());
                tokio::spawn(async move {
                    let _permit = permit;
                    // The error text can name a host: only the count is kept.
                    if client_connection(client, &*dialer, &counters).await.is_err() {
                        counters.failed.fetch_add(1, Relaxed);
                    }
                });
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}

async fn client_connection<D: Dialer>(client: TcpStream, dialer: &D, counters: &Counters) -> io::Result<()> {
    client.set_nodelay(true)?;
    let mut client = Buffered::new(client);
    let mut upstream: Option<Upstream> = None;
    loop {
        let head = match tokio::time::timeout(IDLE, client.head(http1::parse_request)).await {
            Err(_) | Ok(Ok(None)) => return Ok(()),
            Ok(Err(e)) => {
                let _ = client.inner.write_all(&http1::status_response(400, "Bad Request")).await;
                return Err(e);
            }
            Ok(Ok(Some(head))) => head,
        };
        client.consume(head.len);
        let target = http1::target(&head);
        let request_body = http1::request_body(&head);
        let (target, request_body) = match (target, request_body) {
            (Some(t), Ok(b)) => (t, b),
            _ => {
                client.inner.write_all(&http1::status_response(400, "Bad Request")).await?;
                return Ok(());
            }
        };
        match target {
            Target::Connect { host, port } => {
                counters.tunnels.fetch_add(1, Relaxed);
                let mut up = match dial(dialer, &host, port).await {
                    Ok(up) => up,
                    Err(e) => {
                        client.inner.write_all(&http1::status_response(502, "Bad Gateway")).await?;
                        return Err(e);
                    }
                };
                client.inner.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n").await?;
                if !client.data().is_empty() {
                    up.conn.inner.write_all(client.data()).await?;
                    let n = client.data().len();
                    client.consume(n);
                }
                tokio::io::copy_bidirectional(&mut client.inner, &mut up.conn.inner).await?;
                return Ok(());
            }
            Target::Absolute { host, port, path } => {
                counters.requests.fetch_add(1, Relaxed);
                let request = http1::origin_request(&head, &host, port, &path);
                let reusable = upstream.take().filter(|u| u.host == host && u.port == port);
                let reused = reusable.is_some();
                let mut up = match reusable {
                    Some(up) => {
                        counters.reused.fetch_add(1, Relaxed);
                        up
                    }
                    None => match dial(dialer, &host, port).await {
                        Ok(up) => up,
                        Err(e) => {
                            client.inner.write_all(&http1::status_response(502, "Bad Gateway")).await?;
                            return Err(e);
                        }
                    },
                };
                let response = match exchange(&mut up, &request, &mut client, request_body, dialer, counters).await {
                    // A kept-alive origin connection that closed while idle: once more on a new
                    // one, if the request had no body to resend.
                    Ok(None) | Err(_) if reused && request_body == Body::None => {
                        up = match dial(dialer, &host, port).await {
                            Ok(up) => up,
                            Err(e) => {
                                client.inner.write_all(&http1::status_response(502, "Bad Gateway")).await?;
                                return Err(e);
                            }
                        };
                        exchange(&mut up, &request, &mut client, Body::None, dialer, counters).await
                    }
                    other => other,
                };
                let response = match response {
                    Ok(Some(r)) => r,
                    Ok(None) => {
                        client.inner.write_all(&http1::status_response(502, "Bad Gateway")).await?;
                        return Err(io::ErrorKind::UnexpectedEof.into());
                    }
                    Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                        client.inner.write_all(&http1::status_response(504, "Gateway Timeout")).await?;
                        return Err(e);
                    }
                    Err(e) => {
                        client.inner.write_all(&http1::status_response(502, "Bad Gateway")).await?;
                        return Err(e);
                    }
                };
                if response.status == 101 {
                    client.inner.write_all(&up.conn.data()[..response.len]).await?;
                    up.conn.consume(response.len);
                    if !up.conn.data().is_empty() {
                        client.inner.write_all(up.conn.data()).await?;
                        let n = up.conn.data().len();
                        up.conn.consume(n);
                    }
                    tokio::io::copy_bidirectional(&mut client.inner, &mut up.conn.inner).await?;
                    return Ok(());
                }
                let body = http1::response_body(&head.method, &response).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                client.inner.write_all(&up.conn.data()[..response.len]).await?;
                up.conn.consume(response.len);
                relay_body(&mut up.conn, &mut client.inner, body).await?;
                let keep = http1::keep_alive(head.version, &head.headers)
                    && http1::keep_alive(response.version, &response.headers)
                    && body != Body::Close;
                if !keep {
                    let _ = client.inner.shutdown().await;
                    return Ok(());
                }
                upstream = Some(up);
            }
        }
    }
}

/// Send a request head and body; the response head that follows.
async fn exchange<D: Dialer>(
    up: &mut Upstream,
    request: &[u8],
    client: &mut Buffered<TcpStream>,
    body: Body,
    dialer: &D,
    counters: &Counters,
) -> io::Result<Option<http1::ResponseHead>> {
    up.conn.inner.write_all(request).await?;
    relay_body(client, &mut up.conn.inner, body).await?;
    response_head(up, dialer, counters).await
}

/// The final response head (interim 1xx heads other than 101 are dropped:
/// q2nd's clients send no Expect). The returned head is still in `up`'s buffer,
/// unconsumed. Waits at most [`RESPONSE_HEAD_LIMIT`].
async fn response_head<D: Dialer>(up: &mut Upstream, dialer: &D, counters: &Counters) -> io::Result<Option<http1::ResponseHead>> {
    let limit = tokio::time::Instant::now() + RESPONSE_HEAD_LIMIT;
    let late = |_| io::Error::new(io::ErrorKind::TimedOut, "no response head");
    loop {
        let head = if up.route == Route::Tailnet {
            match tokio::time::timeout(STALL_AFTER, up.conn.head(http1::parse_response)).await {
                Ok(head) => head?,
                Err(_) => {
                    counters.stalls.fetch_add(1, Relaxed);
                    dialer.stalled();
                    tokio::time::timeout_at(limit, up.conn.head(http1::parse_response)).await.map_err(late)??
                }
            }
        } else {
            tokio::time::timeout_at(limit, up.conn.head(http1::parse_response)).await.map_err(late)??
        };
        match head {
            Some(h) if (100..200).contains(&h.status) && h.status != 101 => up.conn.consume(h.len),
            other => return Ok(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tokio::io::AsyncBufReadExt;

    /// Dials everything on the loopback host, recording each dial.
    struct Loopback {
        dials: Mutex<Vec<(String, u16)>>,
        stalls: AtomicU32,
        route: Route,
    }

    impl Dialer for Loopback {
        fn dial(&self, host: &str, port: u16) -> impl Future<Output = io::Result<Dialed>> + Send {
            self.dials.lock().unwrap().push((host.to_owned(), port));
            let route = self.route;
            async move {
                let conn = tokio::time::timeout(CONNECT_LIMIT, TcpStream::connect(("127.0.0.1", port))).await??;
                Ok(Dialed { conn: Box::new(conn), route })
            }
        }
        fn stalled(&self) {
            self.stalls.fetch_add(1, Relaxed);
        }
    }

    async fn proxy(route: Route) -> (u16, Arc<Loopback>, Arc<Counters>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let dialer = Arc::new(Loopback { dials: Mutex::new(vec![]), stalls: AtomicU32::new(0), route });
        let counters = Arc::new(Counters::default());
        tokio::spawn(serve(listener, dialer.clone(), counters.clone(), None));
        (port, dialer, counters)
    }

    /// An origin that answers each request on a connection with its own canned
    /// responses, in order, and records the requests it read.
    async fn origin(responses: Vec<Vec<u8>>) -> (u16, Arc<Mutex<Vec<String>>>, Arc<AtomicU32>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(AtomicU32::new(0));
        let (seen2, connections2) = (seen.clone(), connections.clone());
        tokio::spawn(async move {
            loop {
                let (s, _) = listener.accept().await.unwrap();
                connections2.fetch_add(1, Relaxed);
                let (seen, responses) = (seen2.clone(), responses.clone());
                tokio::spawn(async move {
                    let mut s = Buffered::new(s);
                    for response in responses {
                        let Ok(Some(head)) = s.head(http1::parse_request).await else { return };
                        s.consume(head.len);
                        let body = http1::request_body(&head).unwrap();
                        let mut sink = Vec::new();
                        relay_body(&mut s, &mut sink, body).await.unwrap();
                        seen.lock().unwrap().push(format!("{} {} {}", head.method, head.target, String::from_utf8_lossy(&sink)));
                        s.inner.write_all(&response).await.unwrap();
                    }
                });
            }
        });
        (port, seen, connections)
    }

    async fn read_response(stream: &mut tokio::io::BufReader<TcpStream>) -> (String, Vec<u8>) {
        let mut head = String::new();
        loop {
            let mut line = String::new();
            stream.read_line(&mut line).await.unwrap();
            head += &line;
            if line == "\r\n" || line.is_empty() {
                break;
            }
        }
        let parsed = http1::parse_response(head.as_bytes()).unwrap().unwrap();
        let mut body = Vec::new();
        match http1::response_body("GET", &parsed).unwrap() {
            Body::Length(n) => {
                body.resize(n as usize, 0);
                stream.read_exact(&mut body).await.unwrap();
            }
            Body::Close => {
                stream.read_to_end(&mut body).await.unwrap();
            }
            Body::Chunked => {
                let mut scanner = http1::Chunked::default();
                loop {
                    let mut line = Vec::new();
                    stream.read_until(b'\n', &mut line).await.unwrap();
                    assert!(!line.is_empty(), "end of stream inside a chunked body");
                    body.extend(&line);
                    if scanner.scan(&line).unwrap().1 {
                        break;
                    }
                }
            }
            Body::None => {}
        }
        (head, body)
    }

    #[tokio::test]
    async fn kept_alive_client_sends_several_requests_on_one_origin_connection() {
        let (origin_port, seen, connections) = origin(vec![
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nfirst".to_vec(),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nsec\r\n3;x=y\r\nond\r\n0\r\nT: 1\r\n\r\n".to_vec(),
            b"HTTP/1.1 204 No Content\r\n\r\n".to_vec(),
        ])
        .await;
        let (port, dialer, counters) = proxy(Route::Direct).await;
        let mut c = tokio::io::BufReader::new(TcpStream::connect(("127.0.0.1", port)).await.unwrap());
        let host = format!("peer:{origin_port}");
        c.get_mut().write_all(format!("GET http://{host}/a HTTP/1.1\r\nHost: {host}\r\nProxy-Connection: keep-alive\r\n\r\n").as_bytes()).await.unwrap();
        assert_eq!(read_response(&mut c).await.1, b"first");
        c.get_mut().write_all(format!("POST http://{host}/b HTTP/1.1\r\nHost: {host}\r\nContent-Length: 4\r\n\r\nbody").as_bytes()).await.unwrap();
        let (_, body) = read_response(&mut c).await;
        assert!(body.starts_with(b"3\r\nsec\r\n") && body.ends_with(b"0\r\nT: 1\r\n\r\n"));
        c.get_mut().write_all(format!("GET http://{host}/c HTTP/1.1\r\nHost: {host}\r\n\r\n").as_bytes()).await.unwrap();
        assert!(read_response(&mut c).await.0.starts_with("HTTP/1.1 204"));
        assert_eq!(*seen.lock().unwrap(), vec!["GET /a ", "POST /b body", "GET /c "]);
        assert_eq!(connections.load(Relaxed), 1, "one origin connection for three requests");
        assert_eq!(dialer.dials.lock().unwrap().len(), 1);
        assert_eq!((counters.requests.load(Relaxed), counters.reused.load(Relaxed)), (3, 2));
    }

    #[tokio::test]
    async fn a_closed_origin_ends_the_client_connection_and_the_next_is_redialled() {
        let (origin_port, _seen, connections) = origin(vec![b"HTTP/1.0 200 OK\r\n\r\nuntil close".to_vec()]).await;
        let (port, _, _) = proxy(Route::Direct).await;
        for _ in 0..2 {
            let mut c = tokio::io::BufReader::new(TcpStream::connect(("127.0.0.1", port)).await.unwrap());
            c.get_mut().write_all(format!("GET http://h:{origin_port}/ HTTP/1.1\r\n\r\n").as_bytes()).await.unwrap();
            // The origin keeps its socket open after its one response; the proxy relays until
            // it closes. The origin task ends after its responses, closing the socket.
            assert_eq!(read_response(&mut c).await.1, b"until close");
        }
        assert_eq!(connections.load(Relaxed), 2);
    }

    #[tokio::test]
    async fn an_idle_origin_close_is_retried_once_on_a_new_connection() {
        // Each origin connection answers one request and closes, though it says keep-alive.
        let (origin_port, seen, connections) = origin(vec![b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".to_vec()]).await;
        let (port, _, _) = proxy(Route::Direct).await;
        let mut c = tokio::io::BufReader::new(TcpStream::connect(("127.0.0.1", port)).await.unwrap());
        for _ in 0..3 {
            c.get_mut().write_all(format!("GET http://h:{origin_port}/ HTTP/1.1\r\n\r\n").as_bytes()).await.unwrap();
            assert_eq!(read_response(&mut c).await.1, b"ok");
        }
        assert_eq!((seen.lock().unwrap().len(), connections.load(Relaxed)), (3, 3));
    }

    #[tokio::test]
    async fn a_closed_gate_leaves_clients_queued_until_it_opens() {
        let (origin_port, _, _) = origin(vec![b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".to_vec()]).await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let dialer = Arc::new(Loopback { dials: Mutex::new(vec![]), stalls: AtomicU32::new(0), route: Route::Direct });
        let (gate, gate_rx) = watch::channel(false);
        tokio::spawn(serve(listener, dialer.clone(), Arc::new(Counters::default()), Some(gate_rx)));
        // The kernel completes the connection into the queue; the proxy does not take it.
        let mut c = tokio::io::BufReader::new(TcpStream::connect(("127.0.0.1", port)).await.unwrap());
        c.get_mut().write_all(format!("GET http://h:{origin_port}/ HTTP/1.1\r\nHost: h\r\n\r\n").as_bytes()).await.unwrap();
        let mut byte = [0u8; 1];
        assert!(tokio::time::timeout(Duration::from_millis(300), c.read(&mut byte)).await.is_err(), "answered while closed");
        assert!(dialer.dials.lock().unwrap().is_empty());
        gate.send_replace(true);
        let (head, body) = read_response(&mut c).await;
        assert!(head.starts_with("HTTP/1.1 200") && body == b"ok", "{head}");
        // Closed again while the proxy waits in accept: the next client waits too.
        gate.send_replace(false);
        tokio::task::yield_now().await;
        let mut d = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        d.write_all(format!("GET http://h:{origin_port}/ HTTP/1.1\r\nHost: h\r\n\r\n").as_bytes()).await.unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(300), d.read(&mut byte)).await.is_err(), "answered while closed");
        assert_eq!(dialer.dials.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn connect_tunnels_and_refusals() {
        let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let echo_port = echo.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = echo.accept().await.unwrap();
            let mut buf = [0; 64];
            let n = s.read(&mut buf).await.unwrap();
            s.write_all(&buf[..n]).await.unwrap();
        });
        let (port, _, counters) = proxy(Route::Direct).await;
        let mut c = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        c.write_all(format!("CONNECT h:{echo_port} HTTP/1.1\r\nHost: h:{echo_port}\r\n\r\nearly").as_bytes()).await.unwrap();
        let mut got = vec![0; 39 + 5];
        c.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"HTTP/1.1 200 Connection established\r\n\r\nearly");
        // Nothing listens on port 1 of the loopback host.
        let mut c = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        c.write_all(b"GET http://h:1/ HTTP/1.1\r\n\r\n").await.unwrap();
        let mut answer = String::new();
        c.read_to_string(&mut answer).await.unwrap();
        assert!(answer.starts_with("HTTP/1.1 502"));
        let mut c = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        c.write_all(b"GET /not-a-proxy-request HTTP/1.1\r\n\r\n").await.unwrap();
        let mut answer = String::new();
        c.read_to_string(&mut answer).await.unwrap();
        assert!(answer.starts_with("HTTP/1.1 400"));
        assert_eq!(counters.tunnels.load(Relaxed), 1);
    }

    #[tokio::test]
    async fn a_silent_origin_gets_504_and_frees_the_slot() {
        let origin = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin_port = origin.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (s, _) = origin.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(30)).await;
            drop(s);
        });
        let (port, _, _) = proxy(Route::Direct).await;
        let mut c = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        c.write_all(format!("GET http://h:{origin_port}/ HTTP/1.1\r\n\r\n").as_bytes()).await.unwrap();
        let mut answer = String::new();
        tokio::time::timeout(RESPONSE_HEAD_LIMIT * 3, c.read_to_string(&mut answer)).await.unwrap().unwrap();
        assert!(answer.starts_with("HTTP/1.1 504"), "{answer}");
    }

    #[tokio::test]
    async fn a_late_tailnet_response_head_is_one_stall() {
        let origin = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin_port = origin.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = origin.accept().await.unwrap();
            let mut buf = [0; 256];
            let _ = s.read(&mut buf).await.unwrap();
            tokio::time::sleep(STALL_AFTER + Duration::from_millis(300)).await;
            s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nlate").await.unwrap();
        });
        let (port, dialer, _) = proxy(Route::Tailnet).await;
        let mut c = tokio::io::BufReader::new(TcpStream::connect(("127.0.0.1", port)).await.unwrap());
        c.get_mut().write_all(format!("GET http://h:{origin_port}/ HTTP/1.1\r\n\r\n").as_bytes()).await.unwrap();
        assert_eq!(read_response(&mut c).await.1, b"late");
        assert_eq!(dialer.stalls.load(Relaxed), 1);
    }
}
