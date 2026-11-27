//! HTTP/1.x message heads and body framing for the proxy: enough to forward
//! several requests on one kept-alive client connection, as `subsonic::http`
//! sends plain-HTTP requests to a proxy (absolute form, pooled connections).
//! Bodies are forwarded byte for byte; only their ends are found here.
use std::fmt;

/// Longest request or response head accepted.
pub const MAX_HEAD: usize = 16 * 1024;
const MAX_HEADERS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Not a valid HTTP/1.x head.
    Malformed,
    /// Longer than [`MAX_HEAD`].
    TooLarge,
    /// A body framing this proxy does not forward (e.g. two lengths).
    Framing,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Error::Malformed => "malformed HTTP head",
            Error::TooLarge => "HTTP head too large",
            Error::Framing => "unsupported HTTP body framing",
        })
    }
}

impl std::error::Error for Error {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub name: String,
    pub value: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestHead {
    pub method: String,
    pub target: String,
    /// The minor version: 0 or 1.
    pub version: u8,
    pub headers: Vec<Header>,
    /// Bytes the head occupied, terminator included.
    pub len: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseHead {
    pub status: u16,
    pub version: u8,
    pub headers: Vec<Header>,
    pub len: usize,
}

fn collect(headers: &[httparse::Header<'_>]) -> Vec<Header> {
    headers.iter().map(|h| Header { name: h.name.to_owned(), value: h.value.to_vec() }).collect()
}

/// A request head at the start of `buf`; `None` while incomplete.
pub fn parse_request(buf: &[u8]) -> Result<Option<RequestHead>, Error> {
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut request = httparse::Request::new(&mut headers);
    match request.parse(buf) {
        Ok(httparse::Status::Complete(len)) => Ok(Some(RequestHead {
            method: request.method.ok_or(Error::Malformed)?.to_owned(),
            target: request.path.ok_or(Error::Malformed)?.to_owned(),
            version: request.version.ok_or(Error::Malformed)?,
            headers: collect(request.headers),
            len,
        })),
        Ok(httparse::Status::Partial) if buf.len() > MAX_HEAD => Err(Error::TooLarge),
        Ok(httparse::Status::Partial) => Ok(None),
        Err(httparse::Error::TooManyHeaders) => Err(Error::TooLarge),
        Err(_) => Err(Error::Malformed),
    }
}

/// A response head at the start of `buf`; `None` while incomplete.
pub fn parse_response(buf: &[u8]) -> Result<Option<ResponseHead>, Error> {
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut response = httparse::Response::new(&mut headers);
    match response.parse(buf) {
        Ok(httparse::Status::Complete(len)) => Ok(Some(ResponseHead {
            status: response.code.ok_or(Error::Malformed)?,
            version: response.version.ok_or(Error::Malformed)?,
            headers: collect(response.headers),
            len,
        })),
        Ok(httparse::Status::Partial) if buf.len() > MAX_HEAD => Err(Error::TooLarge),
        Ok(httparse::Status::Partial) => Ok(None),
        Err(httparse::Error::TooManyHeaders) => Err(Error::TooLarge),
        Err(_) => Err(Error::Malformed),
    }
}

fn values<'a>(headers: &'a [Header], name: &'a str) -> impl Iterator<Item = &'a [u8]> + 'a {
    headers.iter().filter(move |h| h.name.eq_ignore_ascii_case(name)).map(|h| h.value.as_slice())
}

/// Comma-separated tokens of all `name` headers, lowercased.
fn tokens(headers: &[Header], name: &str) -> Vec<String> {
    values(headers, name)
        .flat_map(|v| v.split(|&b| b == b','))
        .map(|t| String::from_utf8_lossy(t).trim().to_ascii_lowercase())
        .filter(|t| !t.is_empty())
        .collect()
}

/// Whether the connection stays open after this message (RFC 9112 9.3).
pub fn keep_alive(version: u8, headers: &[Header]) -> bool {
    let connection = tokens(headers, "connection");
    if connection.iter().any(|t| t == "close") {
        return false;
    }
    version >= 1 || connection.iter().any(|t| t == "keep-alive")
}

/// How a message body ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Body {
    None,
    Length(u64),
    Chunked,
    /// Until the sender closes (responses only).
    Close,
}

fn framing(headers: &[Header]) -> Result<Option<Body>, Error> {
    let coding = tokens(headers, "transfer-encoding");
    if let Some(last) = coding.last() {
        // Chunked must be the final coding; anything else has no length (RFC 9112 6.3).
        return if last == "chunked" { Ok(Some(Body::Chunked)) } else { Err(Error::Framing) };
    }
    let mut length = None;
    for value in values(headers, "content-length") {
        for part in value.split(|&b| b == b',') {
            let text = std::str::from_utf8(part).map_err(|_| Error::Framing)?.trim();
            if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
                return Err(Error::Framing);
            }
            let n: u64 = text.parse().map_err(|_| Error::Framing)?;
            if length.is_some_and(|l| l != n) {
                return Err(Error::Framing);
            }
            length = Some(n);
        }
    }
    Ok(length.map(Body::Length))
}

/// A request's body: chunked, a length, or none.
pub fn request_body(head: &RequestHead) -> Result<Body, Error> {
    Ok(match framing(&head.headers)? {
        Some(Body::Length(0)) | None => Body::None,
        Some(body) => body,
    })
}

/// A response's body, given the request's method.
pub fn response_body(method: &str, head: &ResponseHead) -> Result<Body, Error> {
    if method.eq_ignore_ascii_case("HEAD") || (100..200).contains(&head.status) || head.status == 204 || head.status == 304 {
        return Ok(Body::None);
    }
    Ok(match framing(&head.headers)? {
        Some(Body::Length(0)) => Body::None,
        Some(body) => body,
        None => Body::Close,
    })
}

/// Where a proxy request goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// `CONNECT host:port`: a tunnel.
    Connect { host: String, port: u16 },
    /// `GET http://host[:port]/path`: one forwarded request.
    Absolute { host: String, port: u16, path: String },
}

/// Split `host[:port]`, `[v6]` or `[v6]:port`.
pub fn split_authority(authority: &str, default_port: u16) -> Option<(String, u16)> {
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        let port = match after {
            "" => default_port,
            p => p.strip_prefix(':')?.parse().ok()?,
        };
        return Some((host.to_owned(), port));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => Some((host.to_owned(), port.parse().ok()?)),
        Some(_) => None,
        None => Some((authority.to_owned(), default_port)),
    }
}

/// The request's target, if it is one a proxy serves.
pub fn target(head: &RequestHead) -> Option<Target> {
    if head.method.eq_ignore_ascii_case("CONNECT") {
        let (host, port) = split_authority(&head.target, 443)?;
        return Some(Target::Connect { host, port });
    }
    let rest = head.target.get(..7).filter(|s| s.eq_ignore_ascii_case("http://")).map(|_| &head.target[7..])?;
    let (authority, path) = match rest.find(['/', '?']) {
        Some(at) if rest.as_bytes()[at] == b'/' => (&rest[..at], rest[at..].to_owned()),
        Some(at) => (&rest[..at], format!("/{}", &rest[at..])),
        None => (rest, "/".to_owned()),
    };
    let (host, port) = split_authority(authority, 80)?;
    Some(Target::Absolute { host, port, path })
}

/// The request head as sent to the origin: origin-form target, without the
/// proxy's own hop-by-hop headers, with a Host header.
pub fn origin_request(head: &RequestHead, host: &str, port: u16, path: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(head.len + 32);
    out.extend_from_slice(format!("{} {} HTTP/1.{}\r\n", head.method, path, head.version).as_bytes());
    let mut has_host = false;
    for h in &head.headers {
        if h.name.eq_ignore_ascii_case("proxy-connection") || h.name.eq_ignore_ascii_case("proxy-authorization") {
            continue;
        }
        has_host |= h.name.eq_ignore_ascii_case("host");
        out.extend_from_slice(h.name.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(&h.value);
        out.extend_from_slice(b"\r\n");
    }
    if !has_host {
        let host = if host.contains(':') { format!("[{host}]") } else { host.to_owned() };
        let authority = if port == 80 { host } else { format!("{host}:{port}") };
        out.extend_from_slice(format!("Host: {authority}\r\n").as_bytes());
    }
    out.extend_from_slice(b"\r\n");
    out
}

/// A short response the proxy sends itself; the connection closes after it.
pub fn status_response(code: u16, reason: &str) -> Vec<u8> {
    format!("HTTP/1.1 {code} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").into_bytes()
}

/// Finds the end of a chunked body without changing it.
#[derive(Debug, Clone, Default)]
pub struct Chunked {
    state: State,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum State {
    /// In a chunk-size line: the size so far, whether a digit was seen, whether in an extension.
    #[default]
    SizeStart,
    Size { n: u64, ext: bool },
    SizeLf { n: u64 },
    Data(u64),
    DataCr,
    DataLf,
    /// At the start of a trailer line (or the final empty line).
    TrailerStart,
    TrailerLine,
    TrailerLf { empty: bool },
    Done,
}

impl Chunked {
    /// Consume bytes of the body; returns how many belong to it and whether it ended.
    pub fn scan(&mut self, buf: &[u8]) -> Result<(usize, bool), Error> {
        let mut i = 0;
        while i < buf.len() {
            let b = buf[i];
            self.state = match self.state {
                State::Done => return Ok((i, true)),
                State::SizeStart => match hex(b) {
                    Some(d) => State::Size { n: d, ext: false },
                    None => return Err(Error::Framing),
                },
                State::Size { n, ext } => match (b, ext) {
                    (b'\r', _) => State::SizeLf { n },
                    (b'\n', _) => Self::after_size(n),
                    (_, true) => State::Size { n, ext },
                    (b';', false) | (b' ', false) | (b'\t', false) => State::Size { n, ext: true },
                    (_, false) => match hex(b) {
                        Some(d) if n >> 60 == 0 => State::Size { n: n << 4 | d, ext: false },
                        _ => return Err(Error::Framing),
                    },
                },
                State::SizeLf { n } if b == b'\n' => Self::after_size(n),
                State::SizeLf { .. } => return Err(Error::Framing),
                State::Data(n) => {
                    let take = (buf.len() - i).min(usize::try_from(n).unwrap_or(usize::MAX));
                    i += take;
                    let left = n - take as u64;
                    self.state = if left == 0 { State::DataCr } else { State::Data(left) };
                    continue;
                }
                State::DataCr if b == b'\r' => State::DataLf,
                State::DataCr | State::DataLf if b == b'\n' => State::SizeStart,
                State::DataCr | State::DataLf => return Err(Error::Framing),
                State::TrailerStart => match b {
                    b'\r' => State::TrailerLf { empty: true },
                    b'\n' => State::Done,
                    _ => State::TrailerLine,
                },
                State::TrailerLine => match b {
                    b'\r' => State::TrailerLf { empty: false },
                    b'\n' => State::TrailerStart,
                    _ => State::TrailerLine,
                },
                State::TrailerLf { empty } if b == b'\n' => if empty { State::Done } else { State::TrailerStart },
                State::TrailerLf { .. } => return Err(Error::Framing),
            };
            i += 1;
        }
        Ok((i, self.state == State::Done))
    }

    fn after_size(n: u64) -> State {
        if n == 0 { State::TrailerStart } else { State::Data(n) }
    }
}

fn hex(b: u8) -> Option<u64> {
    (b as char).to_digit(16).map(u64::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn request(text: &str) -> RequestHead {
        parse_request(text.as_bytes()).unwrap().unwrap()
    }

    #[test]
    fn absolute_and_connect_targets() {
        let head = request("GET http://nas:4533/rest/ping.view?u=x HTTP/1.1\r\nHost: nas:4533\r\n\r\n");
        assert_eq!(target(&head), Some(Target::Absolute { host: "nas".into(), port: 4533, path: "/rest/ping.view?u=x".into() }));
        let head = request("GET http://100.64.0.1?x HTTP/1.1\r\n\r\n");
        assert_eq!(target(&head), Some(Target::Absolute { host: "100.64.0.1".into(), port: 80, path: "/?x".into() }));
        let head = request("CONNECT [fd7a:115c:a1e0::1]:8443 HTTP/1.1\r\n\r\n");
        assert_eq!(target(&head), Some(Target::Connect { host: "fd7a:115c:a1e0::1".into(), port: 8443 }));
        assert_eq!(target(&request("GET / HTTP/1.1\r\n\r\n")), None, "origin form is not a proxy request");
        assert_eq!(target(&request("GET https://x/ HTTP/1.1\r\n\r\n")), None);
        assert_eq!(target(&request("GET http://user@x/ HTTP/1.1\r\n\r\n")), None);
        assert_eq!(split_authority("a:b:c", 80), None);
    }

    #[test]
    fn origin_request_drops_proxy_headers_and_keeps_the_rest() {
        let head = request("POST http://h:81/p HTTP/1.1\r\nProxy-Connection: keep-alive\r\nContent-Length: 2\r\nX-A: b\r\n\r\n");
        let out = String::from_utf8(origin_request(&head, "h", 81, "/p")).unwrap();
        assert_eq!(out, "POST /p HTTP/1.1\r\nContent-Length: 2\r\nX-A: b\r\nHost: h:81\r\n\r\n");
        let head = request("GET http://h/ HTTP/1.0\r\nHost: h\r\n\r\n");
        assert_eq!(String::from_utf8(origin_request(&head, "h", 80, "/")).unwrap(), "GET / HTTP/1.0\r\nHost: h\r\n\r\n");
    }

    #[test]
    fn body_framing_and_keep_alive() {
        let get = request("GET http://h/ HTTP/1.1\r\n\r\n");
        assert_eq!(request_body(&get), Ok(Body::None));
        assert!(keep_alive(1, &get.headers));
        let post = request("POST http://h/ HTTP/1.1\r\nContent-Length: 5, 5\r\nConnection: close\r\n\r\n");
        assert_eq!(request_body(&post), Ok(Body::Length(5)));
        assert!(!keep_alive(1, &post.headers));
        let bad = request("POST http://h/ HTTP/1.1\r\nContent-Length: 5\r\nContent-Length: 6\r\n\r\n");
        assert_eq!(request_body(&bad), Err(Error::Framing));
        let chunked = request("POST http://h/ HTTP/1.1\r\nTransfer-Encoding: gzip, chunked\r\nContent-Length: 9\r\n\r\n");
        assert_eq!(request_body(&chunked), Ok(Body::Chunked));
        let response = |t: &str| parse_response(t.as_bytes()).unwrap().unwrap();
        assert_eq!(response_body("HEAD", &response("HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\n")), Ok(Body::None));
        assert_eq!(response_body("GET", &response("HTTP/1.1 304 Not Modified\r\n\r\n")), Ok(Body::None));
        assert_eq!(response_body("GET", &response("HTTP/1.0 200 OK\r\n\r\n")), Ok(Body::Close));
        assert!(!keep_alive(0, &response("HTTP/1.0 200 OK\r\n\r\n").headers));
        assert!(keep_alive(0, &response("HTTP/1.0 200 OK\r\nConnection: Keep-Alive\r\n\r\n").headers));
        assert_eq!(parse_request(b"GET http://h/ HTTP/1.1\r\nHost"), Ok(None));
        assert_eq!(parse_request(b"\x00\x01 nonsense\r\n\r\n"), Err(Error::Malformed));
        assert_eq!(parse_request(&vec![b'a'; MAX_HEAD + 1]), Err(Error::TooLarge));
    }

    #[test]
    fn chunked_end_with_extensions_and_trailers() {
        let body = b"4;name=v\r\nWiki\r\n0\r\nX-T: 1\r\n\r\nNEXT";
        let (used, done) = Chunked::default().scan(body).unwrap();
        assert!(done);
        assert_eq!(&body[used..], b"NEXT");
        assert!(Chunked::default().scan(b"zz\r\n").is_err());
        assert!(Chunked::default().scan(b"4\r\nWikiXX").is_err());
        assert!(Chunked::default().scan(b"fffffffffffffffff\r\n").is_err(), "size overflow");
    }

    /// A chunked encoding of `chunks`, with optional extensions and trailers.
    fn encode(chunks: &[Vec<u8>], ext: bool, trailers: &[String]) -> Vec<u8> {
        let mut out = Vec::new();
        for c in chunks.iter().filter(|c| !c.is_empty()) {
            out.extend(format!("{:x}{}\r\n", c.len(), if ext { ";e=1" } else { "" }).as_bytes());
            out.extend(c);
            out.extend(b"\r\n");
        }
        out.extend(b"0\r\n");
        for t in trailers {
            out.extend(format!("X-{t}: v\r\n").as_bytes());
        }
        out.extend(b"\r\n");
        out
    }

    proptest! {
        /// Split anywhere, the scanner finds exactly the encoded body's end.
        #[test]
        fn chunked_end_is_found_across_any_split(
            chunks in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..300), 0..6),
            ext in any::<bool>(),
            trailers in proptest::collection::vec("[a-z]{1,6}", 0..3),
            tail in proptest::collection::vec(any::<u8>(), 0..20),
            cuts in proptest::collection::vec(0usize..2000, 0..8),
        ) {
            let body = encode(&chunks, ext, &trailers);
            let mut wire = body.clone();
            wire.extend(&tail);
            let mut cuts: Vec<usize> = cuts.into_iter().map(|c| c % (wire.len() + 1)).collect();
            cuts.push(wire.len());
            cuts.sort();
            let mut scanner = Chunked::default();
            let (mut at, mut end) = (0, None);
            for cut in cuts {
                if cut < at || end.is_some() { continue; }
                let (used, done) = scanner.scan(&wire[at..cut]).unwrap();
                at += used;
                if done { end = Some(at); }
            }
            prop_assert_eq!(end, Some(body.len()));
        }

        /// Heads arriving in pieces parse only once complete, with the same result.
        #[test]
        fn heads_parse_the_same_whole_or_partial(
            path in "/[a-z0-9/]{0,40}",
            names in proptest::collection::vec("[A-Za-z][A-Za-z0-9-]{0,12}", 0..8),
            cut in 0usize..400,
        ) {
            let mut text = format!("GET http://peer:4533{path} HTTP/1.1\r\n");
            for (i, n) in names.iter().enumerate() { text += &format!("{n}: v{i}\r\n"); }
            text += "\r\n";
            let whole = parse_request(text.as_bytes()).unwrap().unwrap();
            prop_assert_eq!(whole.len, text.len());
            let cut = cut % text.len();
            prop_assert_eq!(parse_request(&text.as_bytes()[..cut]).unwrap(), None);
            let rebuilt = origin_request(&whole, "peer", 4533, &path);
            let again = parse_request(&rebuilt).unwrap().unwrap();
            prop_assert_eq!(again.target, path);
            let host = names.iter().any(|n| n.eq_ignore_ascii_case("host"));
            prop_assert_eq!(again.headers.len(), names.len() + usize::from(!host), "a Host header is added if absent");
        }
    }
}
