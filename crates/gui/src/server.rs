//! Server mode: serves the web build of this very app to the local network, plus the
//! HTTP API it talks to (see `web`). Printing goes through the same printer thread as the
//! local window, so jobs from everywhere queue up in one line.
//!
//! - `GET  /api/status?printer=<name>` — printers present and that printer's health, JSON;
//! - `POST /api/print?printer=<name>`  — the body is a ready job in the printer's language;
//!   answers once it is sent, with the error as text when it failed;
//! - `GET  /api/sizes`, `GET /api/templates` — the files as they are;
//! - `PUT  /api/templates` — a whole new `templates.toml`, written only if it is valid.
//!
//! There is no authentication: anyone on the network can print and edit templates.

use std::{
    fs,
    io::{Read, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use crate::{
    config,
    printer::{Origin, Service},
    query,
};

const MAX_HEAD: usize = 16 << 10;
const MAX_JOB: usize = 16 << 20;
const MAX_TEMPLATES: usize = 1 << 20;
/// A client that stalls mid-request this long is dropped.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Produced by `build.rs`; empty files when the web build was not made (`scripts/build-web.sh`).
const INDEX_HTML: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/web/index.html"));
const APP_JS: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/web/itmo-labels.js"));
const APP_WASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/web/itmo-labels_bg.wasm"));
const NOT_BUILT: &str = "<!doctype html><meta charset=utf-8><title>ITMO Labels</title>\
    <p>This copy of ITMO Labels was built without its web version: run <code>scripts/build-web.sh</code> and rebuild.";

/// Runs until dropped.
///
/// One thread per connection and one request per connection (`Connection: close`): the
/// clients are a few browser tabs on a LAN, and an idle kept-alive connection would hold a
/// thread for nothing. (tiny_http does keep-alive with a small pool and stalls requests here.)
pub struct Server {
    port: u16,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
}

impl Server {
    pub fn start(port: u16, service: Service, dir: PathBuf) -> Result<Self, String> {
        let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).map_err(|e| e.to_string())?;
        // Port 0 picks a free one.
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let accept = thread::spawn(move || {
            for stream in listener.incoming() {
                if stopping.load(Ordering::Acquire) {
                    return;
                }
                let Ok(stream) = stream else { continue };
                let (service, dir) = (service.clone(), dir.clone());
                thread::spawn(move || serve(stream, &service, &dir));
            }
        });
        Ok(Self { port, stop, accept: Some(accept) })
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // `accept` only returns for a connection: make one, so the thread sees the flag and
        // drops the listener, freeing the port.
        let _ = TcpStream::connect_timeout(&SocketAddr::from((Ipv4Addr::LOCALHOST, self.port)), Duration::from_secs(1));
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
    }
}

/// IPv4 addresses others can reach this machine at; the one on the default route first.
pub fn addresses() -> Vec<Ipv4Addr> {
    let mut all: Vec<Ipv4Addr> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|i| match i.ip() {
            IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_link_local() && !ip.is_unspecified() => Some(ip),
            _ => None,
        })
        .collect();
    all.sort();
    all.dedup();
    if let Some(primary) = default_route() {
        all.retain(|&ip| ip != primary);
        all.insert(0, primary);
    }
    all
}

/// The source address the OS would use to reach the internet. Connecting a UDP socket only
/// picks a route; nothing is sent.
fn default_route() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((Ipv4Addr::new(8, 8, 8, 8), 80)).ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if !ip.is_unspecified() && !ip.is_loopback() => Some(ip),
        _ => None,
    }
}

struct Request {
    method: String,
    path: String,
    query: String,
    body: Vec<u8>,
    peer: IpAddr,
}

struct Response {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

fn reply(status: u16, content_type: &'static str, body: impl Into<Vec<u8>>) -> Response {
    Response { status, content_type, body: body.into() }
}

fn text(status: u16, body: impl Into<String>) -> Response {
    reply(status, "text/plain; charset=utf-8", body.into())
}

fn serve(mut stream: TcpStream, service: &Service, dir: &Path) {
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let response = match read_request(&mut stream) {
        Ok(request) => handle(request, service, dir),
        Err(response) => response,
    };
    let reason = match response.status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        411 => "Length Required",
        413 => "Content Too Large",
        500 => "Internal Server Error",
        _ => "Service Unavailable",
    };
    // `no-cache`: a rebuilt app must not run against a cached older page.
    let head = format!(
        "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n",
        response.status,
        response.content_type,
        response.body.len()
    );
    let _ = stream.write_all(head.as_bytes()).and_then(|()| stream.write_all(&response.body));
}

/// The request line, headers and a `Content-Length` body; chunked bodies are not accepted.
fn read_request(stream: &mut TcpStream) -> Result<Request, Response> {
    let peer = stream.peer_addr().map_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED), |a| a.ip());
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = stream.read(&mut chunk).map_err(|e| text(400, e.to_string()))?;
        if n == 0 {
            return Err(text(400, "connection closed mid-request"));
        }
        buf.extend_from_slice(&chunk[..n]);
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut parsed = httparse::Request::new(&mut headers);
        match parsed.parse(&buf).map_err(|e| text(400, e.to_string()))? {
            httparse::Status::Partial if buf.len() > MAX_HEAD => return Err(text(400, "request head too large")),
            httparse::Status::Partial => continue,
            httparse::Status::Complete(head_len) => {
                let header = |name: &str| {
                    parsed.headers.iter().find(|h| h.name.eq_ignore_ascii_case(name)).map(|h| String::from_utf8_lossy(h.value).into_owned())
                };
                if header("transfer-encoding").is_some() {
                    return Err(text(411, "send a Content-Length"));
                }
                let length: usize = match header("content-length") {
                    None => 0,
                    Some(v) => v.trim().parse().map_err(|_| text(400, "bad Content-Length"))?,
                };
                let target = parsed.path.unwrap_or("/");
                let (path, query) = target.split_once('?').unwrap_or((target, ""));
                let limit = if path == "/api/templates" { MAX_TEMPLATES } else { MAX_JOB };
                if length > limit {
                    return Err(text(413, format!("request body over {limit} bytes")));
                }
                let (method, path, query) = (parsed.method.unwrap_or("").to_owned(), path.to_owned(), query.to_owned());
                let mut body = buf.split_off(head_len);
                body.truncate(length);
                let missing = length - body.len();
                stream.take(missing as u64).read_to_end(&mut body).map_err(|e| text(400, e.to_string()))?;
                if body.len() < length {
                    return Err(text(400, "connection closed mid-body"));
                }
                return Ok(Request { method, path, query, body, peer });
            }
        }
    }
}

fn read_file(path: &Path) -> Response {
    match fs::read_to_string(path) {
        Ok(contents) => text(200, contents),
        Err(e) => text(500, format!("{}: {e}", path.display())),
    }
}

fn handle(request: Request, service: &Service, dir: &Path) -> Response {
    let printer = query::param(&request.query, "printer").filter(|p| !p.is_empty());
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/" | "/index.html") if APP_WASM.is_empty() => reply(200, "text/html; charset=utf-8", NOT_BUILT),
        ("GET", "/" | "/index.html") => reply(200, "text/html; charset=utf-8", INDEX_HTML),
        ("GET", "/itmo-labels.js") => reply(200, "text/javascript", APP_JS),
        ("GET", "/itmo-labels_bg.wasm") => reply(200, "application/wasm", APP_WASM),
        ("GET", "/api/status") => {
            let status = service.status(printer.as_deref());
            reply(200, "application/json", serde_json::to_vec(&status).expect("status serializes"))
        }
        ("POST", "/api/print") => {
            let Some(printer) = printer else { return text(400, "no printer selected") };
            let (done, result) = mpsc::channel();
            service.print(printer, request.body, Origin::Remote(request.peer), move |r| {
                let _ = done.send(r);
            });
            match result.recv() {
                Ok(Ok(())) => text(200, ""),
                Ok(Err(e)) => text(503, e),
                Err(_) => text(503, "printer thread stopped"),
            }
        }
        ("GET", "/api/sizes") => read_file(&dir.join("sizes.toml")),
        ("GET", "/api/templates") => read_file(&dir.join("templates.toml")),
        ("PUT", "/api/templates") => match String::from_utf8(request.body)
            .map_err(|e| e.to_string())
            .and_then(|body| config::parse_templates(&body))
        {
            Ok(templates) => match config::write_templates(&dir.join("templates.toml"), &templates) {
                Ok(()) => text(200, ""),
                Err(e) => text(500, e),
            },
            Err(e) => text(400, e),
        },
        _ => text(404, "not found"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One request over a real socket; the status code and the body.
    fn call(port: u16, method: &str, target: &str, body: &str) -> (u16, String) {
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        write!(stream, "{method} {target} HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let status = response[9..12].parse().unwrap();
        let body = response.split_once("\r\n\r\n").unwrap().1.to_owned();
        (status, body)
    }

    #[test]
    fn serves_the_api_and_frees_the_port() {
        let dir = std::env::temp_dir().join(format!("itmo-labels-server-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let server = Server::start(0, Service::spawn(|| {}), dir.clone()).unwrap();
        let port = server.port();

        let (status, body) = call(port, "GET", "/api/status?printer=x%20y", "");
        assert_eq!(status, 200);
        assert!(serde_json::from_str::<crate::printer::Status>(&body).is_ok(), "{body}");

        assert_eq!(call(port, "PUT", "/api/templates", "[[template]]\nname = \"\"").0, 400, "invalid, not written");
        assert!(!dir.join("templates.toml").exists());
        assert_eq!(call(port, "PUT", "/api/templates", "[[template]]\nname = \"Web\"").0, 200);
        let (status, written) = call(port, "GET", "/api/templates", "");
        assert_eq!(status, 200);
        assert_eq!(config::parse_templates(&written).unwrap()[0].name, "Web");

        assert_eq!(call(port, "POST", "/api/print", "job").0, 400, "no printer");
        assert_eq!(call(port, "GET", "/nope", "").0, 404);

        drop(server);
        let _ = fs::remove_dir_all(&dir);
        assert!(TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).is_ok(), "port freed on drop");
    }
}
