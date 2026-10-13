//! Network seam, update check and remote image cache.
//!
//! Real WinHTTP is exercised against loopback servers started inside the tests; the fetch policy,
//! the update check and the image cache run against the scripted network so every redirect,
//! size, content type and timeout case is reproducible.

use std::io::{BufRead, BufReader, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use ssw_io::net::{
    Accept, BodySink, Downloaded, FetchOptions, Fetched, Http, HttpRequest, HttpResponse,
    MAX_REDIRECTS, NetError, ResponseHead, Scheme, ScriptedHttp, USER_AGENT, Url, WinHttp,
    download, fetch,
};
use ssw_io::remote_images::{
    CACHE_LIMIT_BYTES, IMAGE_TIMEOUT, ImageError, ImageLoad, MAX_IMAGE_BYTES, RemoteImage,
    RemoteImages,
};
use ssw_io::store::{Store, StoreError};
use ssw_io::update::{
    Asset, CHECK_INTERVAL_SECS, CHECK_TIMEOUT, DOWNLOAD_TIMEOUT, MAX_CHECKSUMS_BYTES,
    MAX_INSTALLER_BYTES, RELEASES_URL, Release, UpdateError, UpdateStatus, Version,
    check_for_updates, checksums_asset, download_installer, expected_sha256, installer_asset,
    is_due, last_check, latest_release, parse_release,
};
use tempfile::{TempDir, tempdir};

// ---------------------------------------------------------------------------------------------
// Loopback HTTP server
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Seen {
    target: String,
    headers: Vec<(String, String)>,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

enum Reply {
    Bytes(Vec<u8>),
    /// Send the bytes, then keep the connection open and silent for a while.
    BytesThenStall(Vec<u8>, Duration),
    Stall(Duration),
}

type Handler = dyn Fn(&Seen) -> Reply + Send + Sync;

struct Server {
    port: u16,
    seen: Arc<Mutex<Vec<Seen>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

fn read_request(stream: &TcpStream) -> Option<Seen> {
    let mut reader = BufReader::new(stream);
    let mut first = String::new();
    reader.read_line(&mut first).ok()?;
    let target = first.split_whitespace().nth(1)?.to_owned();
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':')?;
        headers.push((name.to_ascii_lowercase(), value.trim().to_owned()));
    }
    Some(Seen { target, headers })
}

fn serve(mut stream: TcpStream, handler: &Handler, seen: &Mutex<Vec<Seen>>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let Some(request) = read_request(&stream) else {
        return;
    };
    seen.lock().push(request.clone());
    match handler(&request) {
        Reply::Bytes(bytes) => {
            let _ = stream.write_all(&bytes);
            let _ = stream.flush();
            let _ = stream.shutdown(Shutdown::Write);
        }
        Reply::BytesThenStall(bytes, duration) => {
            let _ = stream.write_all(&bytes);
            let _ = stream.flush();
            std::thread::sleep(duration);
        }
        Reply::Stall(duration) => std::thread::sleep(duration),
    }
}

impl Server {
    fn start(handler: impl Fn(&Seen) -> Reply + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let handler: Arc<Handler> = Arc::new(handler);
        let thread = {
            let (seen, stop) = (seen.clone(), stop.clone());
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(stream) = stream else { continue };
                    let (handler, seen) = (handler.clone(), seen.clone());
                    std::thread::spawn(move || serve(stream, handler.as_ref(), &seen));
                }
            })
        };
        Self {
            port,
            seen,
            stop,
            thread: Some(thread),
        }
    }

    fn url(&self, target: &str) -> String {
        format!("http://127.0.0.1:{}{target}", self.port)
    }

    fn requests(&self) -> Vec<Seen> {
        self.seen.lock().clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop so it notices the flag.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn response(status: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {status}\r\n");
    for (name, value) in headers {
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    out.push_str("Connection: close\r\n\r\n");
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

fn image_response(body: &[u8]) -> Vec<u8> {
    response(
        "200 OK",
        &[
            ("Content-Type", "image/png"),
            ("Content-Length", &body.len().to_string()),
        ],
        body,
    )
}

fn redirect(status: &str, location: &str) -> Vec<u8> {
    response(
        status,
        &[("Location", location), ("Content-Length", "0")],
        b"",
    )
}

fn request<'a>(url: &'a Url, accept: Accept, max_body: u64, timeout: Duration) -> HttpRequest<'a> {
    HttpRequest {
        url,
        timeout,
        max_body,
        accept,
    }
}

const LIMIT: u64 = 1024 * 1024;
const FIVE_SECONDS: Duration = Duration::from_secs(5);

fn options(accept: Accept) -> FetchOptions {
    FetchOptions {
        accept,
        max_body: LIMIT,
        timeout: FIVE_SECONDS,
        allow_http: false,
    }
}

fn loopback_options(accept: Accept) -> FetchOptions {
    FetchOptions {
        allow_http: true,
        ..options(accept)
    }
}

// ---------------------------------------------------------------------------------------------
// WinHTTP against loopback
// ---------------------------------------------------------------------------------------------

#[test]
fn winhttp_returns_head_and_body_and_sends_no_cookies_or_credentials() {
    let body = vec![7u8; 50_000];
    let payload = body.clone();
    let server = Server::start(move |_| Reply::Bytes(image_response(&payload)));
    let url = Url::parse(&server.url("/img/x.png?size=2")).unwrap();

    let reply = WinHttp
        .get(&request(&url, Accept::Image, LIMIT, FIVE_SECONDS))
        .unwrap();
    assert_eq!(reply.head.status, 200);
    assert_eq!(reply.head.mime().as_deref(), Some("image/png"));
    assert_eq!(reply.head.content_length(), Some(50_000));
    assert_eq!(reply.body, body);

    let seen = server.requests();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].target, "/img/x.png?size=2");
    assert_eq!(seen[0].header("accept"), Some("image/*"));
    assert_eq!(seen[0].header("user-agent"), Some(USER_AGENT));
    assert_eq!(seen[0].header("cookie"), None);
    assert_eq!(seen[0].header("authorization"), None);
    assert_eq!(
        seen[0].header("host"),
        Some(format!("127.0.0.1:{}", server.port).as_str())
    );
}

#[test]
fn winhttp_reports_redirects_instead_of_following_them() {
    let server = Server::start(|seen| match seen.target.as_str() {
        "/old" => Reply::Bytes(redirect("302 Found", "/new")),
        _ => Reply::Bytes(image_response(b"final")),
    });
    let url = Url::parse(&server.url("/old")).unwrap();
    let reply = WinHttp
        .get(&request(&url, Accept::Image, LIMIT, FIVE_SECONDS))
        .unwrap();
    assert_eq!(reply.head.status, 302);
    assert_eq!(reply.head.header("location"), Some("/new"));
    assert!(reply.body.is_empty());
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn winhttp_does_not_download_bodies_it_does_not_want() {
    let server = Server::start(|seen| match seen.target.as_str() {
        "/page" => Reply::Bytes(response(
            "200 OK",
            &[("Content-Type", "text/html"), ("Content-Length", "300")],
            &[b'x'; 300],
        )),
        _ => Reply::Bytes(response(
            "404 Not Found",
            &[("Content-Type", "image/png"), ("Content-Length", "4")],
            b"nope",
        )),
    });
    let page = Url::parse(&server.url("/page")).unwrap();
    let reply = WinHttp
        .get(&request(&page, Accept::Image, LIMIT, FIVE_SECONDS))
        .unwrap();
    assert_eq!(reply.head.status, 200);
    assert_eq!(reply.head.mime().as_deref(), Some("text/html"));
    assert!(reply.body.is_empty());

    let missing = Url::parse(&server.url("/missing")).unwrap();
    let reply = WinHttp
        .get(&request(&missing, Accept::Image, LIMIT, FIVE_SECONDS))
        .unwrap();
    assert_eq!(reply.head.status, 404);
    assert!(reply.body.is_empty());

    // The policy layer turns both into errors.
    let wrong = fetch(
        &WinHttp,
        &page.to_string(),
        &loopback_options(Accept::Image),
    )
    .unwrap_err();
    assert_eq!(
        wrong,
        NetError::WrongContentType(Some("text/html".to_owned()))
    );
    let gone = fetch(
        &WinHttp,
        &missing.to_string(),
        &loopback_options(Accept::Image),
    )
    .unwrap_err();
    assert_eq!(gone, NetError::HttpStatus(404));
}

#[test]
fn winhttp_enforces_the_size_cap_from_the_header_and_while_streaming() {
    let server = Server::start(|seen| match seen.target.as_str() {
        // Announced size over the cap.
        "/announced" => Reply::Bytes(image_response(&[1u8; 100])),
        // No Content-Length: the size is only known by reading.
        "/streamed" => Reply::Bytes(response(
            "200 OK",
            &[("Content-Type", "image/png")],
            &[2u8; 100],
        )),
        _ => Reply::Bytes(response(
            "200 OK",
            &[("Content-Type", "image/png")],
            &[3u8; 50],
        )),
    });
    let get = |target: &str, max: u64| {
        let url = Url::parse(&server.url(target)).unwrap();
        WinHttp.get(&request(&url, Accept::Image, max, FIVE_SECONDS))
    };
    assert_eq!(
        get("/announced", 50).unwrap_err(),
        NetError::TooLarge { limit: 50 }
    );
    assert_eq!(
        get("/streamed", 50).unwrap_err(),
        NetError::TooLarge { limit: 50 }
    );
    // Exactly at the cap is fine.
    assert_eq!(get("/exact", 50).unwrap().body, vec![3u8; 50]);
    // An empty body is fine too.
    let empty = Server::start(|_| {
        Reply::Bytes(response(
            "200 OK",
            &[("Content-Type", "image/png"), ("Content-Length", "0")],
            b"",
        ))
    });
    let url = Url::parse(&empty.url("/")).unwrap();
    assert!(
        WinHttp
            .get(&request(&url, Accept::Image, 50, FIVE_SECONDS))
            .unwrap()
            .body
            .is_empty()
    );
}

#[test]
fn winhttp_gives_up_on_a_server_that_never_answers() {
    let server = Server::start(|_| Reply::Stall(Duration::from_millis(2500)));
    let url = Url::parse(&server.url("/slow")).unwrap();
    let started = Instant::now();
    let error = WinHttp
        .get(&request(
            &url,
            Accept::Any,
            LIMIT,
            Duration::from_millis(300),
        ))
        .unwrap_err();
    assert_eq!(error, NetError::Timeout);
    assert!(
        started.elapsed() < Duration::from_millis(2000),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn winhttp_gives_up_on_a_body_that_stops_arriving() {
    let server = Server::start(|_| {
        let head = response(
            "200 OK",
            &[("Content-Type", "image/png"), ("Content-Length", "100")],
            &[1u8; 10],
        );
        Reply::BytesThenStall(head, Duration::from_millis(2500))
    });
    let url = Url::parse(&server.url("/half.png")).unwrap();
    let started = Instant::now();
    let error = WinHttp
        .get(&request(
            &url,
            Accept::Image,
            LIMIT,
            Duration::from_millis(400),
        ))
        .unwrap_err();
    assert_eq!(error, NetError::Timeout);
    assert!(
        started.elapsed() < Duration::from_millis(2000),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn a_connection_that_drops_mid_body_never_yields_a_truncated_image() {
    let server = Server::start(|_| {
        Reply::Bytes(response(
            "200 OK",
            &[("Content-Type", "image/png"), ("Content-Length", "100")],
            &[1u8; 10],
        ))
    });
    let error = fetch(
        &WinHttp,
        &server.url("/cut.png"),
        &loopback_options(Accept::Image),
    )
    .unwrap_err();
    // Either WinHTTP reports the broken connection or our length check catches the short body.
    assert!(
        matches!(
            error,
            NetError::Os {
                call: "WinHttpReadData",
                ..
            } | NetError::MalformedResponse(_)
        ),
        "{error:?}"
    );
}

#[test]
fn a_body_that_does_not_match_its_content_length_is_rejected() {
    let http = ScriptedHttp::new();
    for (announced, body) in [("10", &b"short"[..]), ("2", &b"longer than announced"[..])] {
        http.respond(
            IMG,
            &head(
                "200 OK",
                &[("Content-Type", "image/png"), ("Content-Length", announced)],
            ),
            body,
        );
        let error = fetch(&http, IMG, &options(Accept::Image)).unwrap_err();
        assert!(
            matches!(
                error,
                NetError::MalformedResponse("body does not match Content-Length")
            ),
            "{announced}: {error:?}"
        );
    }
}

#[test]
fn a_connection_that_breaks_while_the_body_is_read_reports_that_failure() {
    let http = ScriptedHttp::new();
    let broken = NetError::Os {
        call: "WinHttpReadData",
        code: 12030,
    };
    http.respond_broken(
        IMG,
        &head(
            "200 OK",
            &[("Content-Type", "image/png"), ("Content-Length", "100")],
        ),
        b"first part",
        broken.clone(),
    );
    // The part that arrived is not returned: the whole fetch fails with the break.
    assert_eq!(
        fetch(&http, IMG, &options(Accept::Image)).unwrap_err(),
        broken
    );
    assert_eq!(http.requests(), [IMG]);
}

#[test]
fn winhttp_reports_a_refused_connection_as_an_os_error() {
    let port = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let url = Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap();
    let error = WinHttp
        .get(&request(&url, Accept::Any, LIMIT, FIVE_SECONDS))
        .unwrap_err();
    assert!(matches!(error, NetError::Os { .. }), "{error:?}");
    assert!(error.to_string().contains("failed with error"));
}

#[test]
fn winhttp_uses_tls_for_https_urls() {
    // A listener that drops every connection: a TLS handshake against it must fail.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().take(8) {
            drop(stream);
        }
    });
    let url = Url::parse(&format!("https://127.0.0.1:{port}/")).unwrap();
    assert_eq!(url.scheme(), Scheme::Https);
    let error = WinHttp
        .get(&request(&url, Accept::Any, LIMIT, FIVE_SECONDS))
        .unwrap_err();
    assert!(
        matches!(error, NetError::Os { .. } | NetError::Timeout),
        "{error:?}"
    );
}

#[test]
fn fetch_follows_redirects_over_real_http_when_http_is_allowed() {
    let server = Server::start(|seen| match seen.target.as_str() {
        "/start" => Reply::Bytes(redirect("302 Found", "/mid")),
        "/mid" => Reply::Bytes(redirect("301 Moved Permanently", "final.png")),
        _ => Reply::Bytes(image_response(b"PNGDATA")),
    });
    let fetched = fetch(
        &WinHttp,
        &server.url("/start"),
        &loopback_options(Accept::Image),
    )
    .unwrap();
    assert_eq!(fetched.url.to_string(), server.url("/final.png"));
    assert_eq!(fetched.mime.as_deref(), Some("image/png"));
    assert_eq!(fetched.body, b"PNGDATA");
    let targets: Vec<String> = server.requests().into_iter().map(|s| s.target).collect();
    assert_eq!(targets, ["/start", "/mid", "/final.png"]);
}

#[test]
fn fetch_stops_after_five_redirects() {
    let server = Server::start(|_| Reply::Bytes(redirect("302 Found", "/loop")));
    let error = fetch(
        &WinHttp,
        &server.url("/loop"),
        &loopback_options(Accept::Any),
    )
    .unwrap_err();
    assert_eq!(error, NetError::TooManyRedirects);
    assert_eq!(server.requests().len(), MAX_REDIRECTS + 1);
}

#[test]
fn fetch_never_touches_the_network_for_plain_http_by_default() {
    let server = Server::start(|_| Reply::Bytes(image_response(b"x")));
    let error = fetch(&WinHttp, &server.url("/a.png"), &options(Accept::Image)).unwrap_err();
    assert_eq!(error, NetError::InsecureScheme);
    assert!(server.requests().is_empty());
}

// ---------------------------------------------------------------------------------------------
// Fetch policy over the scripted network
// ---------------------------------------------------------------------------------------------

fn head(status: &str, headers: &[(&str, &str)]) -> String {
    let mut text = format!("HTTP/1.1 {status}\r\n");
    for (name, value) in headers {
        text.push_str(&format!("{name}: {value}\r\n"));
    }
    text.push_str("\r\n");
    text
}

fn png_head() -> String {
    head("200 OK", &[("Content-Type", "image/png")])
}

fn redirect_head(status: &str, location: &str) -> String {
    head(status, &[("Location", location)])
}

const IMG: &str = "https://img.example.com/a.png";

#[test]
fn fetch_returns_the_final_url_type_and_bytes() {
    let http = ScriptedHttp::new();
    http.respond(
        IMG,
        &head(
            "200 OK",
            &[("Content-Type", "Application/JSON; charset=utf-8")],
        ),
        b"{}",
    );
    let fetched = fetch(
        &http,
        "HTTPS://IMG.example.com/a.png#frag",
        &options(Accept::Json),
    )
    .unwrap();
    assert_eq!(
        fetched,
        Fetched {
            url: Url::parse(IMG).unwrap(),
            mime: Some("application/json".to_owned()),
            body: b"{}".to_vec(),
        }
    );
    assert_eq!(http.requests(), [IMG]);
}

#[test]
fn fetch_maps_statuses_and_content_types_to_errors() {
    for (status, expected) in [
        ("404 Not Found", NetError::HttpStatus(404)),
        ("500 Internal Server Error", NetError::HttpStatus(500)),
        ("204 No Content", NetError::HttpStatus(204)),
        ("304 Not Modified", NetError::HttpStatus(304)),
    ] {
        let http = ScriptedHttp::new();
        http.respond(IMG, &head(status, &[("Content-Type", "image/png")]), b"");
        assert_eq!(
            fetch(&http, IMG, &options(Accept::Image)).unwrap_err(),
            expected,
            "{status}"
        );
    }
    let http = ScriptedHttp::new();
    http.respond(
        IMG,
        &head("200 OK", &[("Content-Type", "text/html")]),
        b"<html>",
    );
    assert_eq!(
        fetch(&http, IMG, &options(Accept::Image)).unwrap_err(),
        NetError::WrongContentType(Some("text/html".to_owned()))
    );
    http.respond(IMG, &head("200 OK", &[]), b"bytes");
    assert_eq!(
        fetch(&http, IMG, &options(Accept::Image)).unwrap_err(),
        NetError::WrongContentType(None)
    );
    // `Any` takes whatever comes.
    http.respond(IMG, &head("200 OK", &[]), b"bytes");
    let any = fetch(&http, IMG, &options(Accept::Any)).unwrap();
    assert_eq!((any.mime, any.body), (None, b"bytes".to_vec()));
    // `+json` types count as JSON.
    http.respond(
        IMG,
        &head("200 OK", &[("Content-Type", "application/vnd.github+json")]),
        b"{}",
    );
    assert!(fetch(&http, IMG, &options(Accept::Json)).is_ok());
}

#[test]
fn every_redirect_status_is_followed_and_locations_are_resolved() {
    for status in [
        "301 Moved",
        "302 Found",
        "303 See Other",
        "307 Temporary",
        "308 Permanent",
    ] {
        let http = ScriptedHttp::new();
        http.respond(IMG, &redirect_head(status, "/b/c.png"), b"");
        http.respond("https://img.example.com/b/c.png", &png_head(), b"ok");
        let fetched = fetch(&http, IMG, &options(Accept::Image)).unwrap();
        assert_eq!(fetched.body, b"ok", "{status}");
    }

    let http = ScriptedHttp::new();
    http.respond(
        IMG,
        &redirect_head("302 Found", "//cdn.example.net/p.png#x"),
        b"",
    );
    http.respond(
        "https://cdn.example.net/p.png",
        &redirect_head("302 Found", "q.png"),
        b"",
    );
    http.respond("https://cdn.example.net/q.png", &png_head(), b"cdn");
    let fetched = fetch(&http, IMG, &options(Accept::Image)).unwrap();
    assert_eq!(fetched.url.to_string(), "https://cdn.example.net/q.png");
    assert_eq!(
        http.requests(),
        [
            IMG,
            "https://cdn.example.net/p.png",
            "https://cdn.example.net/q.png"
        ]
    );
}

#[test]
fn redirects_without_a_usable_location_fail() {
    let http = ScriptedHttp::new();
    http.respond(IMG, &head("302 Found", &[]), b"");
    assert_eq!(
        fetch(&http, IMG, &options(Accept::Any)).unwrap_err(),
        NetError::BadRedirect
    );
    http.respond(IMG, &redirect_head("302 Found", "   "), b"");
    assert_eq!(
        fetch(&http, IMG, &options(Accept::Any)).unwrap_err(),
        NetError::BadRedirect
    );
    http.respond(
        IMG,
        &redirect_head("302 Found", "ftp://files.example.com/a.png"),
        b"",
    );
    assert_eq!(
        fetch(&http, IMG, &options(Accept::Any)).unwrap_err(),
        NetError::UnsupportedScheme("ftp".to_owned())
    );
}

#[test]
fn five_redirects_are_allowed_and_a_sixth_is_not() {
    let chain = |length: usize| {
        let http = ScriptedHttp::new();
        for step in 0..length {
            http.respond(
                &format!("https://img.example.com/r{step}"),
                &redirect_head("302 Found", &format!("/r{}", step + 1)),
                b"",
            );
        }
        http.respond(
            &format!("https://img.example.com/r{length}"),
            &png_head(),
            b"end",
        );
        http
    };
    let five = fetch(
        &chain(5),
        "https://img.example.com/r0",
        &options(Accept::Image),
    )
    .unwrap();
    assert_eq!(five.body, b"end");
    let six = fetch(
        &chain(6),
        "https://img.example.com/r0",
        &options(Accept::Image),
    );
    assert_eq!(six.unwrap_err(), NetError::TooManyRedirects);
}

#[test]
fn a_redirect_may_not_downgrade_to_plain_http_unless_http_is_allowed() {
    let http = ScriptedHttp::new();
    http.respond(
        IMG,
        &redirect_head("302 Found", "http://img.example.com/p.png"),
        b"",
    );
    assert_eq!(
        fetch(&http, IMG, &options(Accept::Image)).unwrap_err(),
        NetError::InsecureScheme
    );
    assert_eq!(http.requests(), [IMG], "the http hop was never requested");

    http.respond(
        IMG,
        &redirect_head("302 Found", "http://img.example.com/p.png"),
        b"",
    );
    http.respond("http://img.example.com/p.png", &png_head(), b"plain");
    let allowed = FetchOptions {
        allow_http: true,
        ..options(Accept::Image)
    };
    assert_eq!(fetch(&http, IMG, &allowed).unwrap().body, b"plain");
}

#[test]
fn bad_urls_and_plain_http_are_rejected_before_any_request() {
    let http = ScriptedHttp::new();
    for (url, expected) in [
        ("not a url", NetError::InvalidUrl("missing scheme")),
        (
            "file:///C:/x.png",
            NetError::UnsupportedScheme("file".to_owned()),
        ),
        ("http://img.example.com/a.png", NetError::InsecureScheme),
    ] {
        assert_eq!(
            fetch(&http, url, &options(Accept::Image)).unwrap_err(),
            expected,
            "{url}"
        );
    }
    assert!(http.requests().is_empty());
}

#[test]
fn size_limits_apply_to_announced_and_delivered_bytes() {
    let tight = FetchOptions {
        max_body: 100,
        ..options(Accept::Image)
    };
    let http = ScriptedHttp::new();
    // Announced too large: rejected without reading the body.
    http.respond(
        IMG,
        &head(
            "200 OK",
            &[("Content-Type", "image/png"), ("Content-Length", "101")],
        ),
        b"short",
    );
    assert_eq!(
        fetch(&http, IMG, &tight).unwrap_err(),
        NetError::TooLarge { limit: 100 }
    );
    // Not announced, delivered too large.
    http.respond(IMG, &png_head(), &[0u8; 101]);
    assert_eq!(
        fetch(&http, IMG, &tight).unwrap_err(),
        NetError::TooLarge { limit: 100 }
    );
    // Exactly the limit, over several read chunks, arrives intact.
    let big = FetchOptions {
        max_body: 100_000,
        ..options(Accept::Image)
    };
    let body: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
    http.respond(IMG, &png_head(), &body);
    assert_eq!(fetch(&http, IMG, &big).unwrap().body, body);
    // A non-numeric Content-Length is ignored.
    http.respond(
        IMG,
        &head(
            "200 OK",
            &[("Content-Type", "image/png"), ("Content-Length", "many")],
        ),
        b"ok",
    );
    assert_eq!(fetch(&http, IMG, &tight).unwrap().body, b"ok");
}

#[test]
fn the_deadline_covers_the_whole_fetch() {
    let quick = FetchOptions {
        timeout: Duration::from_millis(40),
        ..options(Accept::Image)
    };
    // A body that arrives after the deadline.
    let http = ScriptedHttp::new();
    http.respond_after(IMG, Duration::from_millis(100), &png_head(), b"late");
    assert_eq!(fetch(&http, IMG, &quick).unwrap_err(), NetError::Timeout);

    // A redirect answered too slowly leaves no time for the next hop, which is never requested.
    let http = ScriptedHttp::new();
    http.respond_after(
        IMG,
        Duration::from_millis(100),
        &redirect_head("302 Found", "/b.png"),
        b"",
    );
    http.respond("https://img.example.com/b.png", &png_head(), b"never");
    assert_eq!(fetch(&http, IMG, &quick).unwrap_err(), NetError::Timeout);
    assert_eq!(http.requests(), [IMG]);

    // A zero deadline fails before the first request.
    let none = FetchOptions {
        timeout: Duration::ZERO,
        ..options(Accept::Image)
    };
    let http = ScriptedHttp::new();
    assert_eq!(fetch(&http, IMG, &none).unwrap_err(), NetError::Timeout);
    assert!(http.requests().is_empty());
}

#[test]
fn scripted_failures_unreachable_hosts_and_reply_order() {
    let http = ScriptedHttp::new();
    // Nothing scripted: like an unreachable host.
    assert_eq!(
        fetch(&http, IMG, &options(Accept::Any)).unwrap_err(),
        NetError::Os {
            call: "connect",
            code: 12029
        }
    );
    http.fail(IMG, NetError::Timeout);
    http.respond(IMG, &head("503 Unavailable", &[]), b"");
    http.respond(IMG, &png_head(), b"third time lucky");
    assert_eq!(
        fetch(&http, IMG, &options(Accept::Image)).unwrap_err(),
        NetError::Timeout
    );
    assert_eq!(
        fetch(&http, IMG, &options(Accept::Image)).unwrap_err(),
        NetError::HttpStatus(503)
    );
    assert_eq!(
        fetch(&http, IMG, &options(Accept::Image)).unwrap().body,
        b"third time lucky"
    );
    assert_eq!(http.requests().len(), 4);
}

#[test]
fn malformed_response_heads_are_rejected() {
    for (raw, reason) in [
        ("", "missing status line"),
        ("garbage\r\n\r\n", "missing status line"),
        ("HTTP/1.1\r\n\r\n", "invalid status code"),
        ("HTTP/1.1 abc OK\r\n\r\n", "invalid status code"),
        ("HTTP/1.1 99 Too Low\r\n\r\n", "invalid status code"),
        ("HTTP/1.1 600 Too High\r\n\r\n", "invalid status code"),
        (
            "HTTP/1.1 200 OK\r\nthis line has no colon\r\n\r\n",
            "header line without a colon",
        ),
    ] {
        let error = ResponseHead::parse(raw).unwrap_err();
        assert_eq!(error, NetError::MalformedResponse(reason), "{raw:?}");
        assert!(error.to_string().contains(reason));
        // The same text served by the scripted network fails the same way.
        let http = ScriptedHttp::new();
        http.respond(IMG, raw, b"");
        assert_eq!(fetch(&http, IMG, &options(Accept::Any)).unwrap_err(), error);
    }
}

// ---------------------------------------------------------------------------------------------
// Url, ResponseHead, Accept, NetError
// ---------------------------------------------------------------------------------------------

#[test]
fn urls_are_normalized() {
    let cases = [
        (
            "https://Example.COM/a/b?c=d#frag",
            "https://example.com/a/b?c=d",
            "example.com",
            443,
            "/a/b?c=d",
        ),
        (
            "http://example.com",
            "http://example.com/",
            "example.com",
            80,
            "/",
        ),
        (
            "  HTTPS://EXAMPLE.com:8443/x  ",
            "https://example.com:8443/x",
            "example.com",
            8443,
            "/x",
        ),
        (
            "https://example.com?q=1",
            "https://example.com/?q=1",
            "example.com",
            443,
            "/?q=1",
        ),
        (
            "https://[::1]:8080/p",
            "https://[::1]:8080/p",
            "::1",
            8080,
            "/p",
        ),
        (
            "https://[2001:db8::2]/",
            "https://[2001:db8::2]/",
            "2001:db8::2",
            443,
            "/",
        ),
        (
            "https://example.com/a b/ü?x=ç",
            "https://example.com/a%20b/%C3%BC?x=%C3%A7",
            "example.com",
            443,
            "/a%20b/%C3%BC?x=%C3%A7",
        ),
        (
            "https://example.com/a%20b|{}",
            "https://example.com/a%20b%7C%7B%7D",
            "example.com",
            443,
            "/a%20b%7C%7B%7D",
        ),
        (
            "https://exa_mple-1.example.com/",
            "https://exa_mple-1.example.com/",
            "exa_mple-1.example.com",
            443,
            "/",
        ),
    ];
    for (input, text, host, port, target) in cases {
        let url = Url::parse(input).unwrap();
        assert_eq!(url.to_string(), text, "{input}");
        assert_eq!(
            (url.host(), url.port(), url.target()),
            (host, port, target),
            "{input}"
        );
        // Normalizing is idempotent.
        assert_eq!(Url::parse(text).unwrap(), url, "{input}");
    }
    assert_eq!(
        Url::parse("http://a.example/").unwrap().scheme(),
        Scheme::Http
    );
    assert_eq!(
        Url::parse("https://a.example/").unwrap().scheme(),
        Scheme::Https
    );
}

/// A text sink that takes `room` pieces and then refuses.
struct Refusing {
    room: usize,
    text: String,
}

impl std::fmt::Write for Refusing {
    fn write_str(&mut self, piece: &str) -> std::fmt::Result {
        if self.room == 0 {
            return Err(std::fmt::Error);
        }
        self.room -= 1;
        self.text.push_str(piece);
        Ok(())
    }
}

#[test]
fn a_sink_that_refuses_text_fails_the_url_at_whichever_piece_it_stops() {
    // A default port, an explicit port and an IPv6 literal take different paths through the
    // formatting.
    for text in [
        "https://example.com/a?b=c",
        "https://example.com:8443/a",
        "https://[::1]:8080/p",
    ] {
        let url = Url::parse(text).unwrap();
        let mut written = None;
        for room in 0..16 {
            let mut sink = Refusing {
                room,
                text: String::new(),
            };
            if std::fmt::write(&mut sink, format_args!("{url}")).is_ok() {
                written = Some((room, sink.text));
                break;
            }
            assert!(
                text.starts_with(&sink.text) && sink.text.len() < text.len(),
                "{text}: only a prefix was written, got {:?}",
                sink.text
            );
        }
        let (room, full) = written.expect("a big enough sink takes the whole URL");
        assert_eq!(full, text);
        assert!(room > 3, "{text} is written in several pieces, not {room}");
    }
}

#[test]
fn invalid_urls_are_rejected_with_a_reason() {
    let cases = [
        ("example.com/x", NetError::InvalidUrl("missing scheme")),
        ("", NetError::InvalidUrl("missing scheme")),
        (
            "ftp://example.com/",
            NetError::UnsupportedScheme("ftp".to_owned()),
        ),
        (
            "https://user:pw@example.com/",
            NetError::InvalidUrl("credentials are not allowed"),
        ),
        ("https:///x", NetError::InvalidUrl("invalid host")),
        (
            "https://exa mple.com/",
            NetError::InvalidUrl("invalid host"),
        ),
        (
            "https://ünal.example/",
            NetError::InvalidUrl("invalid host"),
        ),
        ("https://::1/", NetError::InvalidUrl("invalid host")),
        (
            "https://example.com:0/",
            NetError::InvalidUrl("invalid port"),
        ),
        (
            "https://example.com:99999/",
            NetError::InvalidUrl("invalid port"),
        ),
        (
            "https://example.com:abc/",
            NetError::InvalidUrl("invalid port"),
        ),
        (
            "https://[::1/",
            NetError::InvalidUrl("unterminated IPv6 literal"),
        ),
        (
            "https://[zzz]/",
            NetError::InvalidUrl("invalid IPv6 literal"),
        ),
        ("https://[::1]x/", NetError::InvalidUrl("invalid port")),
        ("https://[::1]:0/", NetError::InvalidUrl("invalid port")),
    ];
    for (input, expected) in cases {
        assert_eq!(Url::parse(input).unwrap_err(), expected, "{input:?}");
    }
}

#[test]
fn locations_are_resolved_against_the_current_url() {
    let base = Url::parse("https://example.com/dir/page.html?x=1").unwrap();
    let cases = [
        ("https://other.org/z#f", "https://other.org/z"),
        ("http://insecure.example/", "http://insecure.example/"),
        ("//cdn.example.net/a.png", "https://cdn.example.net/a.png"),
        ("/abs/path.png", "https://example.com/abs/path.png"),
        ("/a/./b/../c", "https://example.com/a/c"),
        ("/redir?u=http://x", "https://example.com/redir?u=http://x"),
        ("other.png", "https://example.com/dir/other.png"),
        ("../up.png", "https://example.com/up.png"),
        ("../../../way/up.png", "https://example.com/way/up.png"),
        ("./same.png?y=2", "https://example.com/dir/same.png?y=2"),
        ("?y=2", "https://example.com/dir/page.html?y=2"),
        ("sub/", "https://example.com/dir/sub/"),
        (".", "https://example.com/dir/"),
        ("..", "https://example.com/"),
        ("a b.png", "https://example.com/dir/a%20b.png"),
        ("  /trimmed  ", "https://example.com/trimmed"),
    ];
    for (location, expected) in cases {
        assert_eq!(
            base.join(location).unwrap().to_string(),
            expected,
            "{location:?}"
        );
    }
    for empty in ["", "   ", "#only-a-fragment"] {
        assert_eq!(
            base.join(empty).unwrap_err(),
            NetError::BadRedirect,
            "{empty:?}"
        );
    }
    assert_eq!(
        base.join("ftp://x/").unwrap_err(),
        NetError::UnsupportedScheme("ftp".to_owned())
    );
    // The scheme-relative form keeps the scheme of the current URL.
    let plain = Url::parse("http://example.com/").unwrap();
    assert_eq!(
        plain.join("//x.example/").unwrap().to_string(),
        "http://x.example/"
    );
}

#[test]
fn response_heads_are_parsed_leniently() {
    let head = ResponseHead::parse(
        "HTTP/1.1 200 OK\r\nContent-Type: Image/PNG; charset=binary\r\nCONTENT-LENGTH: 42\r\nX-Dup: first\r\nx-dup: second\r\n\r\nignored: after the blank line\r\n",
    )
    .unwrap();
    assert_eq!(head.status, 200);
    assert_eq!(head.mime().as_deref(), Some("image/png"));
    assert_eq!(head.content_length(), Some(42));
    assert_eq!(head.header("X-DUP"), Some("first"));
    assert_eq!(head.header("ignored"), None);
    assert_eq!(head.headers.len(), 4);

    // Bare LF line ends and an HTTP/2 status line are fine too.
    let lf = ResponseHead::parse("HTTP/2 301\nLocation:  /next \n").unwrap();
    assert_eq!((lf.status, lf.header("location")), (301, Some("/next")));

    // Absent, empty and non-numeric values.
    let bare = ResponseHead::parse(
        "HTTP/1.1 200 OK\r\nContent-Type:  ; charset=x\r\nContent-Length: abc\r\n",
    )
    .unwrap();
    assert_eq!(bare.mime(), None);
    assert_eq!(bare.content_length(), None);
    let none = ResponseHead::parse("HTTP/1.1 200 OK\r\n").unwrap();
    assert_eq!(
        (none.mime(), none.content_length(), none.header("x")),
        (None, None, None)
    );
}

#[test]
fn accept_decides_header_and_acceptable_types() {
    assert_eq!(Accept::Any.header_value(), "*/*");
    assert_eq!(Accept::Image.header_value(), "image/*");
    assert_eq!(Accept::Json.header_value(), "application/json");
    let cases = [
        (Accept::Any, None, true),
        (Accept::Any, Some("text/plain"), true),
        (Accept::Image, Some("image/webp"), true),
        (Accept::Image, Some("image/svg+xml"), true),
        (Accept::Image, Some("text/html"), false),
        (Accept::Image, None, false),
        (Accept::Json, Some("application/json"), true),
        (Accept::Json, Some("application/vnd.github+json"), true),
        (Accept::Json, Some("text/json"), false),
        (Accept::Json, None, false),
    ];
    for (accept, mime, expected) in cases {
        assert_eq!(accept.allows(mime), expected, "{accept:?} {mime:?}");
    }
}

#[test]
fn net_errors_describe_themselves() {
    let cases = [
        (
            NetError::InvalidUrl("missing scheme"),
            "invalid URL: missing scheme",
        ),
        (NetError::UnsupportedScheme("ftp".to_owned()), "ftp"),
        (NetError::InsecureScheme, "plain http"),
        (NetError::TooManyRedirects, "more than 5 redirects"),
        (NetError::BadRedirect, "Location"),
        (NetError::HttpStatus(404), "404"),
        (
            NetError::WrongContentType(Some("text/html".to_owned())),
            "text/html",
        ),
        (NetError::TooLarge { limit: 7 }, "7 bytes"),
        (NetError::Timeout, "timed out"),
        (
            NetError::MalformedResponse("bad"),
            "malformed response: bad",
        ),
        (
            NetError::Os {
                call: "WinHttpOpen",
                code: 12007,
            },
            "WinHttpOpen failed with error 12007",
        ),
    ];
    for (error, expected) in cases {
        assert!(
            error.to_string().contains(expected),
            "{error} vs {expected}"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Update check
// ---------------------------------------------------------------------------------------------

const NOW: i64 = 1_800_000_000;
const DAY: i64 = 86_400;

fn store() -> (TempDir, Store) {
    let tmp = tempdir().unwrap();
    let store = Store::open(&tmp.path().join("state.db")).unwrap();
    (tmp, store)
}

fn version(text: &str) -> Version {
    Version::parse(text).unwrap()
}

fn release_json(tag: &str, page: &str) -> String {
    format!(
        r#"{{"tag_name":"{tag}","html_url":"{page}","name":"ignored","draft":false,"prerelease":false,"assets":[]}}"#
    )
}

fn json_head() -> String {
    head(
        "200 OK",
        &[("Content-Type", "application/json; charset=utf-8")],
    )
}

const PAGE: &str = "https://github.com/isolmaz/ss-writer/releases/tag/v0.2.0";

fn v020() -> Release {
    Release {
        version: version("0.2.0"),
        tag: "v0.2.0".to_owned(),
        page_url: PAGE.to_owned(),
        assets: Vec::new(),
    }
}

#[test]
fn versions_parse_in_the_common_spellings() {
    for (text, shown) in [
        ("1.2.3", "1.2.3"),
        ("v1.2.3", "1.2.3"),
        ("V0.0.0", "0.0.0"),
        ("  1.2.3  ", "1.2.3"),
        ("1.2.3-rc.1", "1.2.3-rc.1"),
        ("1.2.3-alpha.beta.2-x", "1.2.3-alpha.beta.2-x"),
        ("1.2.3+build.5", "1.2.3"),
        ("1.2.3-rc.1+build.5", "1.2.3-rc.1"),
        ("10.20.30", "10.20.30"),
    ] {
        let parsed = version(text);
        assert_eq!(parsed.to_string(), shown, "{text}");
    }
    let rc = version("1.2.3-rc.1");
    assert!(rc.is_prerelease());
    assert_eq!((rc.major, rc.minor, rc.patch), (1, 2, 3));
    assert!(!version("1.2.3+build").is_prerelease());
    assert_eq!(version("1.2.3+a"), version("1.2.3+b"));
}

#[test]
fn a_sink_that_refuses_text_fails_the_version_at_whichever_piece_it_stops() {
    // Numeric and text pre-release identifiers are written by different arms.
    let text = "1.2.3-alpha.7.beta";
    let parsed = version(text);
    let mut written = None;
    for room in 0..32 {
        let mut sink = Refusing {
            room,
            text: String::new(),
        };
        if std::fmt::write(&mut sink, format_args!("{parsed}")).is_ok() {
            written = Some((room, sink.text));
            break;
        }
        assert!(
            text.starts_with(&sink.text) && sink.text.len() < text.len(),
            "only a prefix was written, got {:?}",
            sink.text
        );
    }
    let (room, full) = written.expect("a big enough sink takes the whole version");
    assert_eq!(full, text);
    assert!(
        room > 6,
        "the version is written in several pieces, not {room}"
    );
}

#[test]
fn malformed_versions_are_rejected() {
    for text in [
        "",
        "1",
        "1.2",
        "1.2.3.4",
        "a.b.c",
        "1.2.x",
        "01.2.3",
        "1.02.3",
        "1.2.03",
        "-1.2.3",
        "1.2.3-",
        "1.2.3-rc..1",
        "1.2.3-01",
        "1.2.3-rc_1",
        "1.2.3 beta",
        "99999999999999999999.0.0",
        "v",
        "1.2.3-é",
    ] {
        let error = Version::parse(text).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("`{text}` is not a semantic version"),
            "{text:?}"
        );
    }
}

#[test]
fn version_precedence_follows_semver() {
    // The ordering example of the semver specification, plus a few plain comparisons.
    let ordered = [
        "0.9.9",
        "1.0.0-alpha",
        "1.0.0-alpha.1",
        "1.0.0-alpha.beta",
        "1.0.0-beta",
        "1.0.0-beta.2",
        "1.0.0-beta.11",
        "1.0.0-rc.1",
        "1.0.0",
        "1.0.1",
        "1.1.0",
        "2.0.0",
        "10.0.0",
    ];
    for window in ordered.windows(2) {
        let (low, high) = (version(window[0]), version(window[1]));
        assert!(low < high, "{} < {}", window[0], window[1]);
        assert!(high > low);
        assert_ne!(low, high);
    }
    assert_eq!(
        version("1.0.0").cmp(&version("v1.0.0+meta")),
        std::cmp::Ordering::Equal
    );
    assert_eq!(
        version("1.0.0").partial_cmp(&version("1.0.0")),
        Some(std::cmp::Ordering::Equal)
    );
}

#[test]
fn checks_are_due_weekly() {
    for (last, expected) in [
        (None, true),
        (Some(NOW), false),
        (Some(NOW - DAY), false),
        (Some(NOW - CHECK_INTERVAL_SECS + 1), false),
        (Some(NOW - CHECK_INTERVAL_SECS), true),
        (Some(NOW - 30 * DAY), true),
        // The clock moved backwards since the last check.
        (Some(NOW + DAY), true),
        (Some(i64::MAX), true),
    ] {
        assert_eq!(is_due(last, NOW), expected, "{last:?}");
    }
    assert_eq!(CHECK_INTERVAL_SECS, 7 * DAY);
}

#[test]
fn release_json_is_validated() {
    assert_eq!(
        parse_release(release_json("v0.2.0", PAGE).as_bytes()).unwrap(),
        v020()
    );
    assert_eq!(
        parse_release(br#"{"tag_name":"1.0.0-rc.1","html_url":"https://github.com/x"}"#)
            .unwrap()
            .version,
        version("1.0.0-rc.1")
    );

    assert!(matches!(
        parse_release(b"not json").unwrap_err(),
        UpdateError::Json(_)
    ));
    assert!(matches!(
        parse_release(br#"{"tag_name":"v1.0.0"}"#).unwrap_err(),
        UpdateError::Json(_)
    ));
    assert!(matches!(
        parse_release(release_json("latest", PAGE).as_bytes()).unwrap_err(),
        UpdateError::Version(_)
    ));
    for page in [
        "http://github.com/x",
        "javascript:alert(1)",
        "not a url",
        "ftp://github.com/x",
        "",
    ] {
        assert!(
            matches!(
                parse_release(release_json("v1.0.0", page).as_bytes()).unwrap_err(),
                UpdateError::InsecurePage
            ),
            "{page:?}"
        );
    }
}

#[test]
fn a_newer_release_is_reported_once_a_week() {
    let (_tmp, store) = store();
    let http = ScriptedHttp::new();
    let current = version("0.1.0");
    let check = |now: i64| check_for_updates(&store, &http, RELEASES_URL, &current, true, now);

    assert_eq!(last_check(&store).unwrap(), None);
    http.respond(
        RELEASES_URL,
        &json_head(),
        release_json("v0.2.0", PAGE).as_bytes(),
    );
    assert_eq!(check(NOW).unwrap(), UpdateStatus::Available(v020()));
    assert_eq!(last_check(&store).unwrap(), Some(NOW));

    // Within the week nothing is requested (the script is empty: a request would fail).
    assert_eq!(check(NOW + DAY).unwrap(), UpdateStatus::NotDue);
    assert_eq!(
        check(NOW + CHECK_INTERVAL_SECS - 1).unwrap(),
        UpdateStatus::NotDue
    );
    assert_eq!(http.requests().len(), 1);

    // A week later the feed is asked again; the same or an older release is "up to date".
    http.respond(
        RELEASES_URL,
        &json_head(),
        release_json("v0.1.0", PAGE).as_bytes(),
    );
    assert_eq!(
        check(NOW + CHECK_INTERVAL_SECS).unwrap(),
        UpdateStatus::UpToDate
    );
    assert_eq!(last_check(&store).unwrap(), Some(NOW + CHECK_INTERVAL_SECS));
    http.respond(
        RELEASES_URL,
        &json_head(),
        release_json("v0.0.9", PAGE).as_bytes(),
    );
    assert_eq!(
        check(NOW + 2 * CHECK_INTERVAL_SECS).unwrap(),
        UpdateStatus::UpToDate
    );
    assert_eq!(http.requests().len(), 3);
}

#[test]
fn a_disabled_check_does_nothing() {
    let (_tmp, store) = store();
    let http = ScriptedHttp::new();
    let status =
        check_for_updates(&store, &http, RELEASES_URL, &version("0.1.0"), false, NOW).unwrap();
    assert_eq!(status, UpdateStatus::Disabled);
    assert!(http.requests().is_empty());
    assert_eq!(last_check(&store).unwrap(), None);
}

#[test]
fn the_latest_release_is_asked_for_without_schedule_or_record() {
    let (_tmp, store) = store();
    let http = ScriptedHttp::new();
    for _ in 0..2 {
        http.respond(
            RELEASES_URL,
            &json_head(),
            release_json("v0.2.0", PAGE).as_bytes(),
        );
    }
    // Asking twice in a row works: there is no weekly gate, and nothing is recorded.
    assert_eq!(latest_release(&http, RELEASES_URL).unwrap(), v020());
    assert_eq!(latest_release(&http, RELEASES_URL).unwrap(), v020());
    assert_eq!(last_check(&store).unwrap(), None);
    assert_eq!(http.requests().len(), 2);
    // Failures are reported as they are.
    assert!(matches!(
        latest_release(&http, RELEASES_URL).unwrap_err(),
        UpdateError::Net(NetError::Os { .. })
    ));
    assert!(matches!(
        latest_release(&http, "http://api.example.com/latest").unwrap_err(),
        UpdateError::Net(NetError::InsecureScheme)
    ));
}

#[test]
fn failed_checks_are_not_recorded_so_the_next_start_retries() {
    let (_tmp, store) = store();
    let http = ScriptedHttp::new();
    let current = version("0.1.0");
    let check = || check_for_updates(&store, &http, RELEASES_URL, &current, true, NOW);

    // No route to the feed.
    assert!(matches!(
        check().unwrap_err(),
        UpdateError::Net(NetError::Os { .. })
    ));
    // The feed answers with an error status, the wrong type, invalid JSON, an unusable tag.
    http.respond(RELEASES_URL, &head("403 Forbidden", &[]), b"");
    assert!(matches!(
        check().unwrap_err(),
        UpdateError::Net(NetError::HttpStatus(403))
    ));
    http.respond(
        RELEASES_URL,
        &head("200 OK", &[("Content-Type", "text/html")]),
        b"<html>",
    );
    assert!(matches!(
        check().unwrap_err(),
        UpdateError::Net(NetError::WrongContentType(_))
    ));
    http.respond(RELEASES_URL, &json_head(), b"{ nope");
    assert!(matches!(check().unwrap_err(), UpdateError::Json(_)));
    http.respond(
        RELEASES_URL,
        &json_head(),
        release_json("nightly", PAGE).as_bytes(),
    );
    assert!(matches!(check().unwrap_err(), UpdateError::Version(_)));
    assert_eq!(last_check(&store).unwrap(), None);

    // A damaged stored timestamp counts as "never checked".
    store.kv_set("update.last_check", "yesterday").unwrap();
    assert_eq!(last_check(&store).unwrap(), None);
    http.respond(
        RELEASES_URL,
        &json_head(),
        release_json("v0.1.0", PAGE).as_bytes(),
    );
    assert_eq!(check().unwrap(), UpdateStatus::UpToDate);
}

#[test]
fn the_feed_must_be_https() {
    let (_tmp, store) = store();
    let http = ScriptedHttp::new();
    let error = check_for_updates(
        &store,
        &http,
        "http://api.example.com/latest",
        &version("0.1.0"),
        true,
        NOW,
    )
    .unwrap_err();
    assert!(matches!(error, UpdateError::Net(NetError::InsecureScheme)));
    assert!(http.requests().is_empty());
}

/// The real WinHTTP transport talking to a loopback server that stands in for GitHub.
#[derive(Debug)]
struct Loopback {
    port: u16,
}

impl Loopback {
    /// The address on the loopback server of the same path and query as `url`.
    fn rewrite(&self, url: &Url) -> Url {
        Url::parse(&format!("http://127.0.0.1:{}{}", self.port, url.target())).unwrap()
    }
}

impl Http for Loopback {
    fn get(&self, request: &HttpRequest<'_>) -> Result<HttpResponse, NetError> {
        WinHttp.get(&HttpRequest {
            url: &self.rewrite(request.url),
            ..*request
        })
    }

    fn get_streaming(
        &self,
        request: &HttpRequest<'_>,
        sink: &mut BodySink<'_>,
    ) -> Result<ResponseHead, NetError> {
        WinHttp.get_streaming(
            &HttpRequest {
                url: &self.rewrite(request.url),
                ..*request
            },
            sink,
        )
    }
}

#[test]
fn the_update_check_works_end_to_end_over_real_http() {
    let server = Server::start(|seen| {
        assert_eq!(seen.target, "/repos/isolmaz/ss-writer/releases/latest");
        let body = release_json("v0.2.0", PAGE);
        Reply::Bytes(response(
            "200 OK",
            &[
                ("Content-Type", "application/json; charset=utf-8"),
                ("Content-Length", &body.len().to_string()),
            ],
            body.as_bytes(),
        ))
    });
    let (_tmp, store) = store();
    let status = check_for_updates(
        &store,
        &Loopback { port: server.port },
        RELEASES_URL,
        &version("0.1.0"),
        true,
        NOW,
    )
    .unwrap();
    assert_eq!(status, UpdateStatus::Available(v020()));
    let seen = server.requests();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].header("accept"), Some("application/json"));
    assert_eq!(seen[0].header("user-agent"), Some(USER_AGENT));
    assert_eq!(last_check(&store).unwrap(), Some(NOW));
}

#[test]
fn update_errors_describe_themselves() {
    let json = serde_json::from_str::<Vec<u8>>("x").unwrap_err();
    let cases = [
        (UpdateError::Net(NetError::Timeout), "timed out"),
        (UpdateError::Store(StoreError::Cancelled), "cancelled"),
        (UpdateError::Json(json), "invalid JSON"),
        (
            UpdateError::Version(Version::parse("nightly").unwrap_err()),
            "nightly",
        ),
        (UpdateError::InsecurePage, "not an https URL"),
        (
            UpdateError::InsecureAsset("a.msi".to_owned()),
            "`a.msi` is not an https URL",
        ),
        (
            UpdateError::MissingAsset("a.msi".to_owned()),
            "no `a.msi` to download",
        ),
        (
            UpdateError::MissingChecksum("a.msi".to_owned()),
            "SHA256SUMS.txt does not list `a.msi`",
        ),
        (
            UpdateError::ChecksumMismatch {
                name: "a.msi".to_owned(),
                expected: "aa".to_owned(),
                actual: "bb".to_owned(),
            },
            "`a.msi` does not match its checksum (expected aa, got bb)",
        ),
        (
            UpdateError::Io {
                path: PathBuf::from("updates"),
                source: std::io::Error::other("disk full"),
            },
            "updates: disk full",
        ),
    ];
    for (error, expected) in cases {
        assert!(
            error.to_string().contains(expected),
            "{error} vs {expected}"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Streaming and installer downloads
// ---------------------------------------------------------------------------------------------

const PORTABLE: &str = "SSWriter-0.2.0-x64-portable.zip";
const MSI: &str = "SSWriter-0.2.0-x64.msi";
const SUMS: &str = "SHA256SUMS.txt";
const DOWNLOADS: &str = "https://github.com/isolmaz/ss-writer/releases/download/v0.2.0";
const ASSET: &str =
    "https://github.com/isolmaz/ss-writer/releases/download/v0.2.0/SSWriter-0.2.0-x64.msi";
const CDN: &str =
    "https://release-assets.githubusercontent.com/github-production-release-asset/42/installer";
const SUMS_TARGET: &str = "/isolmaz/ss-writer/releases/download/v0.2.0/SHA256SUMS.txt";
const MSI_TARGET: &str = "/isolmaz/ss-writer/releases/download/v0.2.0/SSWriter-0.2.0-x64.msi";
const CDN_TARGET: &str = "/github-production-release-asset/42/installer";

/// The SHA-256 of `abc` (FIPS 180-2, appendix B) and of nothing.
const ABC_SHA256: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// What a release `v0.2.0` of the project carries, in the order the API lists it.
const ASSETS: [(&str, u64); 6] = [
    (PORTABLE, 41_943_040),
    ("isolmaz.SSWriter.installer.yaml", 1_024),
    (MSI, 36_700_160),
    ("isolmaz.SSWriter.locale.en-US.yaml", 812),
    (SUMS, 245),
    ("isolmaz.SSWriter.yaml", 407),
];

/// `len` bytes that differ from place to place, so a lost, repeated or misplaced chunk changes
/// the file and its digest.
fn payload(len: usize) -> Vec<u8> {
    (0..len)
        .map(|index| u8::try_from(index * 31 % 251).unwrap())
        .collect()
}

/// The SHA-256 of `bytes` as `sha256sum` prints it.
fn sha256_of(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn octet_head(length: usize) -> String {
    head(
        "200 OK",
        &[
            ("Content-Type", "application/octet-stream"),
            ("Content-Length", &length.to_string()),
        ],
    )
}

fn octet_response(body: &[u8]) -> Vec<u8> {
    response(
        "200 OK",
        &[
            ("Content-Type", "application/octet-stream"),
            ("Content-Length", &body.len().to_string()),
        ],
        body,
    )
}

fn text_head() -> String {
    head("200 OK", &[("Content-Type", "text/plain; charset=utf-8")])
}

/// The release feed's answer for `tag` with `assets` attached.
fn release_with(tag: &str, assets: &[(&str, u64)]) -> Release {
    let listed: Vec<String> = assets
        .iter()
        .map(|(name, size)| {
            format!(
                r#"{{"name":"{name}","browser_download_url":"{DOWNLOADS}/{name}","size":{size},"content_type":"application/octet-stream","state":"uploaded"}}"#
            )
        })
        .collect();
    let json = format!(
        r#"{{"tag_name":"{tag}","html_url":"{PAGE}","assets":[{}]}}"#,
        listed.join(",")
    );
    parse_release(json.as_bytes()).unwrap()
}

/// Whether `seen`, the progress reports of one download, grows with every report, carries
/// `total` and ends at `end`; a body of the sizes used here arrives in several chunks.
fn assert_progress(seen: &[(u64, Option<u64>)], total: Option<u64>, end: u64) {
    assert!(seen.len() > 1, "expected several chunks: {seen:?}");
    assert!(
        seen.windows(2).all(|pair| pair[0].0 < pair[1].0),
        "progress must grow: {seen:?}"
    );
    assert!(
        seen.iter().all(|&(_, announced)| announced == total),
        "{seen:?}"
    );
    assert_eq!(seen.last().map(|&(downloaded, _)| downloaded), Some(end));
}

#[test]
fn a_streamed_download_writes_the_exact_bytes_and_reports_monotonic_progress() {
    let body = payload(300_000);
    let served = body.clone();
    let server = Server::start(move |_| Reply::Bytes(octet_response(&served)));
    let dir = tempdir().unwrap();
    let target = dir.path().join("installer.msi");
    let url = server.url("/installer.msi");
    let mut seen = Vec::new();

    let done = download(
        &WinHttp,
        &url,
        &loopback_options(Accept::Any),
        &target,
        &mut |downloaded, total| seen.push((downloaded, total)),
    )
    .unwrap();

    assert_eq!(std::fs::read(&target).unwrap(), body);
    assert_eq!(
        done,
        Downloaded {
            url: Url::parse(&url).unwrap(),
            bytes: 300_000,
            sha256: sha256_of(&body),
        }
    );
    assert_progress(&seen, Some(300_000), 300_000);
    assert_eq!(cached_names(dir.path()), ["installer.msi"]);
}

#[test]
fn the_digest_is_the_sha256_of_what_was_written_and_an_unannounced_total_stays_unknown() {
    let http = ScriptedHttp::new();
    let no_length = head("200 OK", &[("Content-Type", "application/octet-stream")]);
    let big = payload(100_000);
    http.respond(ASSET, &no_length, b"abc");
    http.respond(ASSET, &no_length, &big);
    http.respond(ASSET, &octet_head(0), b"");
    let dir = tempdir().unwrap();
    let target = dir.path().join("a.bin");
    let mut seen = Vec::new();
    let run = |seen: &mut Vec<(u64, Option<u64>)>| {
        seen.clear();
        download(
            &http,
            ASSET,
            &options(Accept::Any),
            &target,
            &mut |downloaded, total| seen.push((downloaded, total)),
        )
        .unwrap()
    };

    let done = run(&mut seen);
    assert_eq!((done.bytes, done.sha256.as_str()), (3, ABC_SHA256));
    assert_eq!(seen, [(3, None)]);

    // A longer body replaces the first file.
    let done = run(&mut seen);
    assert_eq!((done.bytes, done.sha256), (100_000, sha256_of(&big)));
    assert_progress(&seen, None, 100_000);
    assert_eq!(std::fs::read(&target).unwrap(), big);

    // An empty body is a complete download too; nothing is reported for it.
    let done = run(&mut seen);
    assert_eq!((done.bytes, done.sha256.as_str()), (0, EMPTY_SHA256));
    assert!(seen.is_empty());
    assert!(std::fs::read(&target).unwrap().is_empty());
    assert_eq!(cached_names(dir.path()), ["a.bin"]);
}

#[test]
fn a_download_follows_the_redirect_to_the_asset_host() {
    let http = ScriptedHttp::new();
    http.respond(ASSET, &redirect_head("302 Found", CDN), b"");
    http.respond(CDN, &octet_head(3), b"abc");
    let dir = tempdir().unwrap();
    let target = dir.path().join("SSWriter-0.2.0-x64.msi");

    let done = download(&http, ASSET, &options(Accept::Any), &target, &mut |_, _| {}).unwrap();

    assert_eq!(done.url, Url::parse(CDN).unwrap());
    assert_eq!(done.sha256, ABC_SHA256);
    assert_eq!(std::fs::read(&target).unwrap(), b"abc");
    assert_eq!(http.requests(), [ASSET, CDN]);
    assert_eq!(cached_names(dir.path()), ["SSWriter-0.2.0-x64.msi"]);
}

#[test]
fn a_download_follows_the_fetch_policy_and_creates_nothing_when_it_is_refused() {
    let dir = tempdir().unwrap();
    let target = dir.path().join("a.bin");
    let run = |http: &ScriptedHttp, url: &str, rules: &FetchOptions| {
        download(http, url, rules, &target, &mut |_, _| {}).unwrap_err()
    };

    let http = ScriptedHttp::new();
    assert_eq!(
        run(&http, "http://example.com/a.bin", &options(Accept::Any)),
        NetError::InsecureScheme
    );
    assert!(http.requests().is_empty());

    http.respond(ASSET, &head("404 Not Found", &[]), b"");
    assert_eq!(
        run(&http, ASSET, &options(Accept::Any)),
        NetError::HttpStatus(404)
    );

    http.respond(
        ASSET,
        &head("200 OK", &[("Content-Type", "text/html")]),
        b"<html>",
    );
    assert_eq!(
        run(&http, ASSET, &options(Accept::Image)),
        NetError::WrongContentType(Some("text/html".to_owned()))
    );

    http.respond(
        ASSET,
        &redirect_head("302 Found", "http://example.com/a.bin"),
        b"",
    );
    assert_eq!(
        run(&http, ASSET, &options(Accept::Any)),
        NetError::InsecureScheme
    );

    http.respond_after(ASSET, Duration::from_millis(200), &octet_head(3), b"abc");
    let quick = FetchOptions {
        timeout: Duration::from_millis(40),
        ..options(Accept::Any)
    };
    assert_eq!(run(&http, ASSET, &quick), NetError::Timeout);

    assert!(cached_names(dir.path()).is_empty());
}

#[test]
fn a_failed_download_leaves_an_earlier_file_alone_and_no_partial_one_behind() {
    let dir = tempdir().unwrap();
    let target = dir.path().join("installer.msi");
    std::fs::write(&target, b"old installer").unwrap();
    let http = ScriptedHttp::new();
    let broken = NetError::Os {
        call: "WinHttpReadData",
        code: 12030,
    };
    http.respond_broken(
        ASSET,
        &octet_head(100_000),
        &payload(40_000),
        broken.clone(),
    );
    http.respond(ASSET, &octet_head(100), &[1u8; 10]);
    let mut seen = Vec::new();
    let run = |seen: &mut Vec<(u64, Option<u64>)>| {
        download(
            &http,
            ASSET,
            &options(Accept::Any),
            &target,
            &mut |downloaded, total| seen.push((downloaded, total)),
        )
    };

    // The connection breaks after some chunks were written.
    assert_eq!(run(&mut seen).unwrap_err(), broken);
    assert_eq!(seen.last().map(|&(downloaded, _)| downloaded), Some(40_000));
    // The connection ends early without an error: the short body is caught.
    assert_eq!(
        run(&mut seen).unwrap_err(),
        NetError::MalformedResponse("body does not match Content-Length")
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"old installer");
    assert_eq!(cached_names(dir.path()), ["installer.msi"]);

    // A complete download replaces it.
    http.respond(ASSET, &octet_head(1000), &payload(1000));
    run(&mut seen).unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), payload(1000));
    assert_eq!(cached_names(dir.path()), ["installer.msi"]);
}

#[test]
fn a_target_that_cannot_be_created_fails_with_the_os_error() {
    let dir = tempdir().unwrap();
    let http = ScriptedHttp::new();
    http.respond(ASSET, &octet_head(3), b"abc");
    let error = download(
        &http,
        ASSET,
        &options(Accept::Any),
        &dir.path().join("missing").join("a.bin"),
        &mut |_, _| {},
    )
    .unwrap_err();
    // ERROR_PATH_NOT_FOUND
    assert_eq!(
        error,
        NetError::Os {
            call: "CreateFileW",
            code: 3
        }
    );
}

#[test]
fn a_body_over_the_limit_fails_with_too_large_and_leaves_no_file_behind() {
    let server = Server::start(|seen| match seen.target.as_str() {
        // Announced size over the limit.
        "/announced" => Reply::Bytes(octet_response(&payload(100_000))),
        // No Content-Length: the size is only known by reading.
        "/streamed" => Reply::Bytes(response(
            "200 OK",
            &[("Content-Type", "application/octet-stream")],
            &payload(100_000),
        )),
        _ => Reply::Bytes(octet_response(&payload(50_000))),
    });
    let dir = tempdir().unwrap();
    let tight = FetchOptions {
        max_body: 50_000,
        ..loopback_options(Accept::Any)
    };
    for target in ["/announced", "/streamed"] {
        let mut seen = Vec::new();
        let error = download(
            &WinHttp,
            &server.url(target),
            &tight,
            &dir.path().join("a.bin"),
            &mut |downloaded, total| seen.push((downloaded, total)),
        )
        .unwrap_err();
        assert_eq!(error, NetError::TooLarge { limit: 50_000 }, "{target}");
        // No chunk beyond the limit reached the file.
        assert!(seen.iter().all(|&(downloaded, _)| downloaded <= 50_000));
        assert!(cached_names(dir.path()).is_empty(), "{target}");
    }

    // Exactly at the limit is fine.
    let exact = dir.path().join("exact.bin");
    let done = download(
        &WinHttp,
        &server.url("/exact"),
        &tight,
        &exact,
        &mut |_, _| {},
    )
    .unwrap();
    assert_eq!(done.bytes, 50_000);
    assert_eq!(std::fs::read(&exact).unwrap(), payload(50_000));
}

#[test]
fn winhttp_hands_the_body_to_the_sink_in_chunks_and_returns_the_head() {
    let body = payload(200_000);
    let served = body.clone();
    let server = Server::start(move |_| Reply::Bytes(octet_response(&served)));
    let url = Url::parse(&server.url("/big")).unwrap();
    let mut received = Vec::new();
    let mut totals = Vec::new();

    let head = WinHttp
        .get_streaming(
            &request(&url, Accept::Any, LIMIT, FIVE_SECONDS),
            &mut |chunk, total| {
                received.extend_from_slice(chunk);
                totals.push(total);
                Ok(())
            },
        )
        .unwrap();

    assert_eq!(head.status, 200);
    assert_eq!(head.mime().as_deref(), Some("application/octet-stream"));
    assert_eq!(received, body);
    assert!(totals.len() > 1, "{} chunks", totals.len());
    assert!(totals.iter().all(|&total| total == Some(200_000)));
}

#[test]
fn a_sink_that_fails_stops_the_stream_with_its_error() {
    let served = payload(200_000);
    let server = Server::start(move |_| Reply::Bytes(octet_response(&served)));
    let scripted = ScriptedHttp::new();
    scripted.respond(ASSET, &octet_head(100_000), &payload(100_000));
    let failure = NetError::Os {
        call: "WriteFile",
        code: 112,
    };
    let networks: [(&dyn Http, String); 2] = [
        (&WinHttp, server.url("/big")),
        (&scripted, ASSET.to_owned()),
    ];
    for (http, url) in networks {
        let url = Url::parse(&url).unwrap();
        let mut calls = 0;
        let result = http.get_streaming(
            &request(&url, Accept::Any, LIMIT, FIVE_SECONDS),
            &mut |_, _| {
                calls += 1;
                if calls == 2 {
                    Err(failure.clone())
                } else {
                    Ok(())
                }
            },
        );
        assert_eq!(result.unwrap_err(), failure, "{url}");
        assert_eq!(
            calls, 2,
            "no chunk is delivered after the sink failed: {url}"
        );
    }
}

#[test]
fn winhttp_streaming_gives_up_on_a_body_that_stops_arriving() {
    let server = Server::start(|_| {
        let head = response(
            "200 OK",
            &[
                ("Content-Type", "application/octet-stream"),
                ("Content-Length", "100000"),
            ],
            &payload(40_000),
        );
        Reply::BytesThenStall(head, Duration::from_millis(2500))
    });
    let url = Url::parse(&server.url("/half")).unwrap();
    let mut received = 0;
    let started = Instant::now();

    let error = WinHttp
        .get_streaming(
            &request(&url, Accept::Any, LIMIT, Duration::from_millis(400)),
            &mut |chunk, _| {
                received += chunk.len();
                Ok(())
            },
        )
        .unwrap_err();

    assert_eq!(error, NetError::Timeout);
    // The chunks that arrived before the stall were handed over, the rest never came.
    assert!(
        (1..=40_000).contains(&received),
        "{received} bytes reached the sink"
    );
    assert!(
        started.elapsed() < Duration::from_millis(2000),
        "{:?}",
        started.elapsed()
    );
}

/// A network that implements only `get`.
#[derive(Debug)]
struct GetOnly {
    raw_head: String,
    body: Vec<u8>,
}

impl Http for GetOnly {
    fn get(&self, _request: &HttpRequest<'_>) -> Result<HttpResponse, NetError> {
        Ok(HttpResponse {
            head: ResponseHead::parse(&self.raw_head)?,
            body: self.body.clone(),
        })
    }
}

#[test]
fn a_network_that_only_implements_get_still_streams_to_a_file() {
    let body = payload(70_000);
    let http = GetOnly {
        raw_head: octet_head(body.len()),
        body: body.clone(),
    };
    let dir = tempdir().unwrap();
    let target = dir.path().join("a.bin");
    let mut seen = Vec::new();

    let done = download(
        &http,
        ASSET,
        &options(Accept::Any),
        &target,
        &mut |downloaded, total| seen.push((downloaded, total)),
    )
    .unwrap();

    assert_eq!(std::fs::read(&target).unwrap(), body);
    assert_eq!(done.sha256, sha256_of(&body));
    assert_progress(&seen, Some(70_000), 70_000);

    // A response whose body is not wanted hands nothing to the sink.
    let refused = GetOnly {
        raw_head: head("404 Not Found", &[]),
        body: Vec::new(),
    };
    let url = Url::parse(ASSET).unwrap();
    let mut calls = 0;
    let answered = refused
        .get_streaming(
            &request(&url, Accept::Any, LIMIT, FIVE_SECONDS),
            &mut |_, _| {
                calls += 1;
                Ok(())
            },
        )
        .unwrap();
    assert_eq!((answered.status, calls), (404, 0));
}

/// A network whose `get` fails; it keeps the default `get_streaming`.
#[derive(Debug)]
struct GetFails;

impl Http for GetFails {
    fn get(&self, _request: &HttpRequest<'_>) -> Result<HttpResponse, NetError> {
        Err(NetError::Timeout)
    }
}

#[test]
fn the_default_stream_passes_on_a_failed_get_and_a_refused_chunk() {
    let url = Url::parse(ASSET).unwrap();
    let ask = request(&url, Accept::Any, LIMIT, FIVE_SECONDS);

    let error = GetFails
        .get_streaming(&ask, &mut |_, _| Ok(()))
        .unwrap_err();
    assert_eq!(error, NetError::Timeout);

    // A sink that refuses stops the stream at its first chunk.
    let body = payload(70_000);
    let http = GetOnly {
        raw_head: octet_head(body.len()),
        body,
    };
    let mut calls = 0;
    let error = http
        .get_streaming(&ask, &mut |_, _| {
            calls += 1;
            Err(NetError::TooLarge { limit: 1 })
        })
        .unwrap_err();
    assert_eq!((error, calls), (NetError::TooLarge { limit: 1 }, 1));
}

#[test]
fn a_download_whose_target_cannot_be_written_fails_and_keeps_nothing() {
    let body = payload(40_000);
    let http = ScriptedHttp::new();
    http.respond(ASSET, &octet_head(body.len()), &body);
    let dir = tempdir().unwrap();

    // The finished file cannot replace a folder that has something in it.
    let target = dir.path().join("busy");
    std::fs::create_dir(&target).unwrap();
    std::fs::write(target.join("inside"), b"x").unwrap();
    let error = download(&http, ASSET, &options(Accept::Any), &target, &mut |_, _| {}).unwrap_err();
    assert!(
        matches!(error, NetError::Os { call, .. } if call == "MoveFileExW"),
        "{error:?}"
    );

    // An empty body reaches no sink, so its file is created at the end; below a file there is
    // no folder to create it in.
    let empty = ScriptedHttp::new();
    empty.respond(ASSET, &octet_head(0), b"");
    let file = dir.path().join("a-file");
    std::fs::write(&file, b"x").unwrap();
    let blocked = file.join("inside.bin");
    let error =
        download(&empty, ASSET, &options(Accept::Any), &blocked, &mut |_, _| {}).unwrap_err();
    assert!(
        matches!(error, NetError::Os { call, .. } if call == "CreateFileW"),
        "{error:?}"
    );
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
}

#[cfg(feature = "fault-injection")]
#[test]
fn a_download_reports_a_write_that_the_disk_refuses() {
    use ssw_io::fault;

    let body = payload(40_000);
    let http = ScriptedHttp::new();
    http.respond(ASSET, &octet_head(body.len()), &body);
    let dir = tempdir().unwrap();
    let target = dir.path().join("a.bin");

    fault::arm(0);
    let error = download(&http, ASSET, &options(Accept::Any), &target, &mut |_, _| {}).unwrap_err();
    assert!(fault::tripped(), "the write was reached");
    fault::disarm();

    assert!(
        matches!(error, NetError::Os { call, .. } if call == "WriteFile"),
        "{error:?}"
    );
    assert!(!target.exists(), "nothing is left behind");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn a_stream_reports_what_the_exchange_could_not_start() {
    // Nothing listens on port 1: the exchange never starts, and the scripted network passes
    // on the failure it was told to answer with.
    let url = Url::parse("http://127.0.0.1:1/update.msi").unwrap();
    let error = WinHttp
        .get_streaming(
            &request(&url, Accept::Any, LIMIT, Duration::from_secs(2)),
            &mut |_, _| Ok(()),
        )
        .unwrap_err();
    assert!(matches!(error, NetError::Os { .. }), "{error:?}");

    let http = ScriptedHttp::new();
    http.fail(ASSET, NetError::Timeout);
    let asset = Url::parse(ASSET).unwrap();
    let error = http
        .get_streaming(
            &request(&asset, Accept::Any, LIMIT, FIVE_SECONDS),
            &mut |_, _| Ok(()),
        )
        .unwrap_err();
    assert_eq!(error, NetError::Timeout);
}

#[test]
fn expected_sha256_picks_the_line_of_the_named_file() {
    let (zip, msi, yaml) = (sha256_of(b"zip"), sha256_of(b"msi"), sha256_of(b"yaml"));
    let sums = format!("{zip}  {PORTABLE}\n{msi}  {MSI}\n{yaml}  isolmaz.SSWriter.yaml\n");

    assert_eq!(expected_sha256(&sums, MSI), Some(msi.clone()));
    assert_eq!(expected_sha256(&sums, PORTABLE), Some(zip.clone()));
    assert_eq!(expected_sha256(&sums, "isolmaz.SSWriter.yaml"), Some(yaml));
    // Other names, including parts of listed ones, are not found.
    for name in [
        "SSWriter-0.2.0-x64",
        "x64.msi",
        "sswriter-0.2.0-x64.msi",
        "",
    ] {
        assert_eq!(expected_sha256(&sums, name), None, "{name:?}");
    }
    assert_eq!(expected_sha256("", MSI), None);

    // The digest comes back lower-case whatever case the list uses; CRLF line ends and the
    // binary-mode marker of `sha256sum` do not matter.
    let shouted = format!("{}  {MSI}\r\n", msi.to_uppercase());
    assert_eq!(expected_sha256(&shouted, MSI), Some(msi.clone()));
    let binary = format!("{zip}  {PORTABLE}\r\n{msi} *{MSI}\r\n");
    assert_eq!(expected_sha256(&binary, MSI), Some(msi.clone()));
}

#[test]
fn expected_sha256_ignores_lines_that_are_not_checksums() {
    let (first, second) = (sha256_of(b"first"), sha256_of(b"second"));
    let sums = format!(
        "# release 0.2.0\n\nabc123  {MSI}\n{bad}  {MSI}\n{MSI}\n{first}  {MSI}\n{second}  {MSI}\n",
        bad = "g".repeat(64)
    );
    // Short digests, digests that are not hex and lines without a digest are skipped; of two
    // valid lines for one name the first counts.
    assert_eq!(expected_sha256(&sums, MSI), Some(first));
    assert_eq!(expected_sha256(&sums, "release 0.2.0"), None);
}

#[test]
fn the_installer_and_the_checksum_list_are_picked_out_of_the_release_assets() {
    let release = release_with("v0.2.0", &ASSETS);

    assert_eq!(
        release
            .assets
            .iter()
            .map(|asset| asset.name.as_str())
            .collect::<Vec<_>>(),
        ASSETS.map(|(name, _)| name),
        "every asset, in feed order"
    );
    assert_eq!(
        installer_asset(&release),
        Some(&Asset {
            name: MSI.to_owned(),
            url: format!("{DOWNLOADS}/{MSI}"),
            size: 36_700_160,
        })
    );
    assert_eq!(
        checksums_asset(&release),
        Some(&Asset {
            name: SUMS.to_owned(),
            url: format!("{DOWNLOADS}/{SUMS}"),
            size: 245,
        })
    );
}

#[test]
fn assets_that_are_not_the_installer_or_the_checksum_list_are_not_picked() {
    // Neither the zip nor the winget manifests are the installer, and none is the list.
    let others = release_with(
        "v0.2.0",
        &[
            (PORTABLE, 1),
            ("isolmaz.SSWriter.yaml", 1),
            ("SSWriter-0.2.0-x64.msi.sig", 1),
            ("sswriter-0.2.0-x64.msi", 1),
            ("SHA256SUMS.txt.sig", 1),
        ],
    );
    assert_eq!(installer_asset(&others), None);
    assert_eq!(checksums_asset(&others), None);

    // The installer of another version is not the installer of this release.
    let stale = release_with("v0.3.0", &ASSETS);
    assert_eq!(installer_asset(&stale), None);
    assert!(checksums_asset(&stale).is_some());

    // A release without assets.
    let bare = parse_release(release_json("v0.2.0", PAGE).as_bytes()).unwrap();
    assert_eq!(installer_asset(&bare), None);
    assert_eq!(checksums_asset(&bare), None);
}

#[test]
fn asset_entries_are_validated() {
    let with = |asset: &str| {
        parse_release(
            format!(r#"{{"tag_name":"v0.2.0","html_url":"{PAGE}","assets":[{asset}]}}"#).as_bytes(),
        )
    };
    assert!(matches!(
        with(r#"{"name":"a.msi","browser_download_url":"http://example.com/a.msi","size":1}"#),
        Err(UpdateError::InsecureAsset(name)) if name == "a.msi"
    ));
    assert!(matches!(
        with(r#"{"name":"a.msi","browser_download_url":"not a url","size":1}"#),
        Err(UpdateError::InsecureAsset(_))
    ));
    for incomplete in [
        r#"{"name":"a.msi","browser_download_url":"https://example.com/a.msi"}"#,
        r#"{"browser_download_url":"https://example.com/a.msi","size":1}"#,
        r#"{"name":"a.msi","size":1}"#,
    ] {
        assert!(
            matches!(with(incomplete), Err(UpdateError::Json(_))),
            "{incomplete}"
        );
    }
}

/// A server standing in for GitHub: the checksum list `sums`, and the installer `msi` behind a
/// redirect to the asset host, as GitHub serves it.
fn release_server(sums: String, msi: Vec<u8>) -> Server {
    Server::start(move |seen| match seen.target.as_str() {
        SUMS_TARGET => Reply::Bytes(response(
            "200 OK",
            &[
                ("Content-Type", "text/plain; charset=utf-8"),
                ("Content-Length", &sums.len().to_string()),
            ],
            sums.as_bytes(),
        )),
        MSI_TARGET => Reply::Bytes(redirect(
            "302 Found",
            &format!("https://release-assets.githubusercontent.com{CDN_TARGET}"),
        )),
        CDN_TARGET => Reply::Bytes(octet_response(&msi)),
        _ => Reply::Bytes(response("404 Not Found", &[("Content-Length", "0")], b"")),
    })
}

#[test]
fn the_installer_is_downloaded_and_verified_over_real_http() {
    let msi = payload(250_000);
    let sums = format!(
        "{}  {PORTABLE}\n{}  {MSI}\n",
        sha256_of(b"zip"),
        sha256_of(&msi)
    );
    let server = release_server(sums, msi.clone());
    let release = release_with("v0.2.0", &ASSETS);
    let tmp = tempdir().unwrap();
    // The folder does not exist yet.
    let dir = tmp.path().join("updates");
    let mut seen = Vec::new();

    let path = download_installer(
        &Loopback { port: server.port },
        &release,
        &dir,
        &mut |downloaded, total| seen.push((downloaded, total)),
    )
    .unwrap();

    assert_eq!(path, dir.join(MSI));
    assert_eq!(std::fs::read(&path).unwrap(), msi);
    assert_eq!(cached_names(&dir), [MSI]);
    assert_progress(&seen, Some(250_000), 250_000);
    // The list comes first, so a release that does not cover its installer fails early; the
    // installer follows the redirect to the asset host.
    let targets: Vec<String> = server
        .requests()
        .into_iter()
        .map(|entry| entry.target)
        .collect();
    assert_eq!(targets, [SUMS_TARGET, MSI_TARGET, CDN_TARGET]);
}

#[test]
fn a_download_folder_that_cannot_be_created_fails_after_the_list_is_read() {
    let msi = payload(1_000);
    let server = release_server(format!("{}  {MSI}\n", sha256_of(&msi)), msi);
    let release = release_with("v0.2.0", &ASSETS);
    let tmp = tempdir().unwrap();
    // A file where the folder would go: creating the folder cannot work.
    let blocker = tmp.path().join("updates");
    std::fs::write(&blocker, b"not a folder").unwrap();
    let dir = blocker.join("cache");

    let error = download_installer(
        &Loopback { port: server.port },
        &release,
        &dir,
        &mut |_, _| {},
    )
    .unwrap_err();

    match error {
        UpdateError::Io { path, .. } => assert_eq!(path, dir),
        other => panic!("not an io error: {other:?}"),
    }
    // The list was read, the installer was never requested.
    let targets: Vec<String> = server
        .requests()
        .into_iter()
        .map(|entry| entry.target)
        .collect();
    assert_eq!(targets, [SUMS_TARGET]);
}

#[test]
fn a_download_that_does_not_match_the_checksum_list_fails_and_leaves_no_file() {
    let msi = payload(100_000);
    let listed = sha256_of(b"not the installer");
    let server = release_server(format!("{listed}  {MSI}\n"), msi.clone());
    let release = release_with("v0.2.0", &ASSETS);
    let tmp = tempdir().unwrap();

    let error = download_installer(
        &Loopback { port: server.port },
        &release,
        tmp.path(),
        &mut |_, _| {},
    )
    .unwrap_err();

    match error {
        UpdateError::ChecksumMismatch {
            name,
            expected,
            actual,
        } => {
            assert_eq!(name, MSI);
            assert_eq!(expected, listed);
            assert_eq!(actual, sha256_of(&msi));
        }
        other => panic!("not a checksum mismatch: {other:?}"),
    }
    assert!(cached_names(tmp.path()).is_empty());
}

#[test]
fn a_release_that_cannot_be_verified_or_fetched_is_refused() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    let release = release_with("v0.2.0", &ASSETS);
    let sums_url = format!("{DOWNLOADS}/{SUMS}");
    let msi_url = format!("{DOWNLOADS}/{MSI}");
    let run = |http: &ScriptedHttp, release: &Release| {
        download_installer(http, release, dir, &mut |_, _| {}).unwrap_err()
    };

    // Nothing is requested for a release without an installer or without a list.
    let idle = ScriptedHttp::new();
    let error = run(&idle, &release_with("v0.2.0", &[(SUMS, 10)]));
    assert!(
        matches!(&error, UpdateError::MissingAsset(name) if name == MSI),
        "{error:?}"
    );
    let error = run(&idle, &release_with("v0.2.0", &[(MSI, 10)]));
    assert!(
        matches!(&error, UpdateError::MissingAsset(name) if name == SUMS),
        "{error:?}"
    );
    assert!(idle.requests().is_empty());

    // A list that does not name the installer: the big download never starts.
    let http = ScriptedHttp::new();
    let others = format!("{}  {PORTABLE}\n", sha256_of(b"zip"));
    http.respond(&sums_url, &text_head(), others.as_bytes());
    let error = run(&http, &release);
    assert!(
        matches!(&error, UpdateError::MissingChecksum(name) if name == MSI),
        "{error:?}"
    );
    assert_eq!(http.requests(), [sums_url.as_str()]);

    // A list that cannot be fetched, or is too big.
    let http = ScriptedHttp::new();
    http.respond(&sums_url, &head("404 Not Found", &[]), b"");
    assert!(matches!(
        run(&http, &release),
        UpdateError::Net(NetError::HttpStatus(404))
    ));
    let http = ScriptedHttp::new();
    let oversized = usize::try_from(MAX_CHECKSUMS_BYTES).unwrap() + 1;
    http.respond(&sums_url, &octet_head(oversized), b"");
    assert!(matches!(
        run(&http, &release),
        UpdateError::Net(NetError::TooLarge { limit }) if limit == MAX_CHECKSUMS_BYTES
    ));

    // An installer that cannot be fetched, or announces more than the limit.
    let listed = format!("{ABC_SHA256}  {MSI}\n");
    let http = ScriptedHttp::new();
    http.respond(&sums_url, &text_head(), listed.as_bytes());
    http.respond(&msi_url, &head("404 Not Found", &[]), b"");
    assert!(matches!(
        run(&http, &release),
        UpdateError::Net(NetError::HttpStatus(404))
    ));
    let http = ScriptedHttp::new();
    http.respond(&sums_url, &text_head(), listed.as_bytes());
    let oversized = usize::try_from(MAX_INSTALLER_BYTES).unwrap() + 1;
    http.respond(&msi_url, &octet_head(oversized), b"");
    assert!(matches!(
        run(&http, &release),
        UpdateError::Net(NetError::TooLarge { limit }) if limit == MAX_INSTALLER_BYTES
    ));

    assert!(cached_names(dir).is_empty());
}

/// A network that notes the limits of every request and answers like the scripted one.
#[derive(Debug, Default)]
struct Limits {
    inner: ScriptedHttp,
    asked: Mutex<Vec<(String, Duration, u64)>>,
}

impl Limits {
    fn note(&self, request: &HttpRequest<'_>) {
        self.asked
            .lock()
            .push((request.url.to_string(), request.timeout, request.max_body));
    }
}

impl Http for Limits {
    fn get(&self, request: &HttpRequest<'_>) -> Result<HttpResponse, NetError> {
        self.note(request);
        self.inner.get(request)
    }

    fn get_streaming(
        &self,
        request: &HttpRequest<'_>,
        sink: &mut BodySink<'_>,
    ) -> Result<ResponseHead, NetError> {
        self.note(request);
        self.inner.get_streaming(request, sink)
    }
}

#[test]
fn the_checksum_list_and_the_installer_are_fetched_under_their_own_limits() {
    assert_eq!(CHECK_TIMEOUT, Duration::from_secs(15));
    assert_eq!(DOWNLOAD_TIMEOUT, Duration::from_secs(10 * 60));
    assert_eq!(MAX_INSTALLER_BYTES, 200 * 1024 * 1024);
    let (sums_url, msi_url) = (format!("{DOWNLOADS}/{SUMS}"), format!("{DOWNLOADS}/{MSI}"));
    let net = Limits::default();
    net.inner.respond(
        &sums_url,
        &text_head(),
        format!("{ABC_SHA256}  {MSI}\n").as_bytes(),
    );
    net.inner.respond(&msi_url, &octet_head(3), b"abc");
    let tmp = tempdir().unwrap();

    let path = download_installer(
        &net,
        &release_with("v0.2.0", &ASSETS),
        tmp.path(),
        &mut |_, _| {},
    )
    .unwrap();

    assert_eq!(std::fs::read(path).unwrap(), b"abc");
    let asked = net.asked.lock().clone();
    let urls: Vec<&str> = asked.iter().map(|(url, ..)| url.as_str()).collect();
    assert_eq!(urls, [sums_url.as_str(), msi_url.as_str()]);
    // What is left of a deadline is what remains after the instants that already passed.
    let (_, list_timeout, list_cap) = &asked[0];
    assert_eq!(*list_cap, MAX_CHECKSUMS_BYTES);
    assert!(*list_timeout <= CHECK_TIMEOUT, "{list_timeout:?}");
    let (_, installer_timeout, installer_cap) = &asked[1];
    assert_eq!(*installer_cap, MAX_INSTALLER_BYTES);
    assert!(
        *installer_timeout > Duration::from_secs(9 * 60) && *installer_timeout <= DOWNLOAD_TIMEOUT,
        "{installer_timeout:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// Remote images
// ---------------------------------------------------------------------------------------------

struct Cache {
    _tmp: TempDir,
    store: Store,
    db: PathBuf,
    dir: PathBuf,
    images: RemoteImages,
}

fn cache(limit: u64) -> Cache {
    let tmp = tempdir().unwrap();
    let db = tmp.path().join("state.db");
    let dir = tmp.path().join("cache").join("images");
    Cache {
        store: Store::open(&db).unwrap(),
        images: RemoteImages::with_limit(dir.clone(), limit),
        _tmp: tmp,
        db,
        dir,
    }
}

fn cache_key(normalized_url: &str) -> String {
    blake3::hash(normalized_url.as_bytes()).to_hex().to_string()
}

fn cached_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn rows(db: &Path) -> i64 {
    Connection::open(db)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM image_cache", [], |row| row.get(0))
        .unwrap()
}

fn image(len: usize, fill: u8) -> Vec<u8> {
    vec![fill; len]
}

fn serve_image(http: &ScriptedHttp, url: &str, bytes: &[u8]) {
    http.respond(
        url,
        &head(
            "200 OK",
            &[
                ("Content-Type", "image/png"),
                ("Content-Length", &bytes.len().to_string()),
            ],
        ),
        bytes,
    );
}

fn loaded(result: Result<ImageLoad, ImageError>) -> RemoteImage {
    match result.unwrap() {
        ImageLoad::Loaded(image) => image,
        ImageLoad::Blocked => panic!("expected an image, got Blocked"),
    }
}

#[test]
fn an_https_image_is_downloaded_cached_and_then_served_from_disk() {
    let c = cache(CACHE_LIMIT_BYTES);
    let http = ScriptedHttp::new();
    let url = "https://img.example.com/a.png";
    serve_image(&http, url, &image(100, 1));

    let first = loaded(c.images.load(&c.store, &http, url, false, 10));
    assert_eq!(
        first,
        RemoteImage {
            mime: "image/png".to_owned(),
            bytes: image(100, 1),
            from_cache: false
        }
    );
    // The second load (another spelling of the URL) needs no network: nothing is scripted.
    let second = loaded(c.images.load(
        &c.store,
        &http,
        "HTTPS://IMG.example.com/a.png#frag",
        false,
        20,
    ));
    assert_eq!(second.bytes, image(100, 1));
    assert!(second.from_cache);
    assert_eq!(http.requests().len(), 1);

    assert_eq!(cached_names(&c.dir), [cache_key(url)]);
    assert_eq!(
        std::fs::read(c.dir.join(cache_key(url))).unwrap(),
        image(100, 1)
    );
    let conn = Connection::open(&c.db).unwrap();
    let row: (String, String, i64, i64) = conn
        .query_row(
            "SELECT url, mime, size, last_access FROM image_cache WHERE key = ?1",
            [cache_key(url)],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(row, (url.to_owned(), "image/png".to_owned(), 100, 20));
}

#[test]
fn lookup_download_and_insert_can_run_separately() {
    let c = cache(CACHE_LIMIT_BYTES);
    let http = ScriptedHttp::new();
    let url = "https://img.example.com/steps.png";
    serve_image(&http, url, &image(64, 4));

    // The store is only needed for the first and the last step, not during the download.
    assert_eq!(c.images.lookup(&c.store, url, 1).unwrap(), None);
    let downloaded = loaded(c.images.download(&http, url, false));
    assert!(!downloaded.from_cache);
    c.images.insert(&c.store, url, &downloaded, 2).unwrap();
    let again = c.images.lookup(&c.store, url, 3).unwrap().unwrap();
    assert_eq!(again.bytes, downloaded.bytes);
    assert!(again.from_cache);
    assert_eq!(rows(&c.db), 1);
}

#[test]
fn plain_http_images_are_blocked_unless_the_user_asked() {
    let c = cache(CACHE_LIMIT_BYTES);
    let http = ScriptedHttp::new();
    let url = "http://img.example.com/a.png";
    assert_eq!(
        c.images.load(&c.store, &http, url, false, 1).unwrap(),
        ImageLoad::Blocked
    );
    assert_eq!(
        c.images.download(&http, url, false).unwrap(),
        ImageLoad::Blocked
    );
    assert!(http.requests().is_empty());
    assert!(!c.dir.exists());

    serve_image(&http, url, &image(10, 5));
    let image = loaded(c.images.load(&c.store, &http, url, true, 2));
    assert_eq!(image.bytes, vec![5; 10]);
    assert_eq!(cached_names(&c.dir), [cache_key(url)]);
}

#[test]
fn unacceptable_images_are_not_cached() {
    let c = cache(CACHE_LIMIT_BYTES);
    let http = ScriptedHttp::new();
    let url = "https://img.example.com/a.png";
    let fail = |http: &ScriptedHttp| c.images.load(&c.store, http, url, false, 1).unwrap_err();

    http.respond(
        url,
        &head("200 OK", &[("Content-Type", "text/html")]),
        b"<html>",
    );
    assert!(matches!(
        fail(&http),
        ImageError::Net(NetError::WrongContentType(_))
    ));
    http.respond(url, &head("404 Not Found", &[]), b"");
    assert!(matches!(
        fail(&http),
        ImageError::Net(NetError::HttpStatus(404))
    ));
    http.fail(url, NetError::Timeout);
    assert!(matches!(fail(&http), ImageError::Net(NetError::Timeout)));
    // Over the 20 MB cap, announced and streamed.
    assert_eq!(MAX_IMAGE_BYTES, 20 * 1024 * 1024);
    let huge = vec![0u8; MAX_IMAGE_BYTES as usize + 1];
    serve_image(&http, url, &huge);
    assert!(matches!(
        fail(&http),
        ImageError::Net(NetError::TooLarge { limit }) if limit == MAX_IMAGE_BYTES
    ));
    http.respond(
        url,
        &head("200 OK", &[("Content-Type", "image/png")]),
        &huge,
    );
    assert!(matches!(
        fail(&http),
        ImageError::Net(NetError::TooLarge { .. })
    ));

    let invalid = c
        .images
        .load(&c.store, &http, "not a url", false, 1)
        .unwrap_err();
    assert!(matches!(invalid, ImageError::Net(NetError::InvalidUrl(_))));
    assert!(matches!(
        c.images.lookup(&c.store, "not a url", 1).unwrap_err(),
        ImageError::Net(NetError::InvalidUrl(_))
    ));
    assert_eq!(rows(&c.db), 0);
    assert!(cached_names(&c.dir).is_empty());
    assert_eq!(IMAGE_TIMEOUT, Duration::from_secs(15));
}

#[test]
fn the_least_recently_used_images_are_evicted_to_fit_the_limit() {
    let c = cache(250);
    let http = ScriptedHttp::new();
    let url = |name: &str| format!("https://img.example.com/{name}.png");
    for (name, fill) in [("a", 1u8), ("b", 2), ("c", 3), ("d", 4)] {
        serve_image(&http, &url(name), &image(100, fill));
    }

    loaded(c.images.load(&c.store, &http, &url("a"), false, 1));
    loaded(c.images.load(&c.store, &http, &url("b"), false, 2));
    // Using `a` makes `b` the oldest.
    assert!(c.images.lookup(&c.store, &url("a"), 3).unwrap().is_some());
    loaded(c.images.load(&c.store, &http, &url("c"), false, 4));
    assert_eq!(rows(&c.db), 2);
    assert_eq!(cached_names(&c.dir), {
        let mut names = vec![cache_key(&url("a")), cache_key(&url("c"))];
        names.sort();
        names
    });
    assert!(c.images.lookup(&c.store, &url("b"), 5).unwrap().is_none());

    // Now `a` (last used at 3) is older than `c` (4).
    loaded(c.images.load(&c.store, &http, &url("d"), false, 6));
    assert!(c.images.lookup(&c.store, &url("a"), 7).unwrap().is_none());
    assert!(c.images.lookup(&c.store, &url("c"), 7).unwrap().is_some());
    assert!(c.images.lookup(&c.store, &url("d"), 7).unwrap().is_some());
    assert_eq!(rows(&c.db), 2);
}

#[test]
fn an_image_larger_than_the_whole_cache_is_still_kept() {
    let c = cache(50);
    let http = ScriptedHttp::new();
    let (a, b) = (
        "https://img.example.com/a.png",
        "https://img.example.com/b.png",
    );
    serve_image(&http, a, &image(100, 1));
    serve_image(&http, b, &image(100, 2));
    loaded(c.images.load(&c.store, &http, a, false, 1));
    assert_eq!(cached_names(&c.dir), [cache_key(a)]);
    // The next one pushes the first out but is itself never evicted.
    loaded(c.images.load(&c.store, &http, b, false, 2));
    assert_eq!(cached_names(&c.dir), [cache_key(b)]);
}

#[test]
fn damaged_or_missing_cache_files_are_dropped_and_downloaded_again() {
    let c = cache(CACHE_LIMIT_BYTES);
    let http = ScriptedHttp::new();
    let url = "https://img.example.com/a.png";
    serve_image(&http, url, &image(100, 1));
    loaded(c.images.load(&c.store, &http, url, false, 1));
    let file = c.dir.join(cache_key(url));

    // Truncated on disk: a miss, and the stale row is removed.
    std::fs::write(&file, image(10, 9)).unwrap();
    assert!(c.images.lookup(&c.store, url, 2).unwrap().is_none());
    assert_eq!(rows(&c.db), 0);

    // `load` then fetches a fresh copy and replaces the damaged file.
    serve_image(&http, url, &image(100, 7));
    let fresh = loaded(c.images.load(&c.store, &http, url, false, 3));
    assert!(!fresh.from_cache);
    assert_eq!(std::fs::read(&file).unwrap(), image(100, 7));

    // Deleted from disk: the same.
    std::fs::remove_file(&file).unwrap();
    assert!(c.images.lookup(&c.store, url, 4).unwrap().is_none());
    assert_eq!(rows(&c.db), 0);
}

#[test]
fn eviction_tolerates_files_that_are_already_gone() {
    let c = cache(150);
    let http = ScriptedHttp::new();
    let (a, b) = (
        "https://img.example.com/a.png",
        "https://img.example.com/b.png",
    );
    serve_image(&http, a, &image(100, 1));
    serve_image(&http, b, &image(100, 2));
    loaded(c.images.load(&c.store, &http, a, false, 1));
    std::fs::remove_file(c.dir.join(cache_key(a))).unwrap();
    loaded(c.images.load(&c.store, &http, b, false, 2));
    assert_eq!(rows(&c.db), 1);
    assert_eq!(cached_names(&c.dir), [cache_key(b)]);
}

#[test]
fn cache_io_failures_are_reported_with_the_path() {
    let http = ScriptedHttp::new();
    let (a, b) = (
        "https://img.example.com/a.png",
        "https://img.example.com/b.png",
    );

    // A victim that cannot be deleted (a folder sits where its file should be).
    let c = cache(150);
    serve_image(&http, a, &image(100, 1));
    serve_image(&http, b, &image(100, 2));
    loaded(c.images.load(&c.store, &http, a, false, 1));
    let victim = c.dir.join(cache_key(a));
    std::fs::remove_file(&victim).unwrap();
    std::fs::create_dir(&victim).unwrap();
    let error = c.images.load(&c.store, &http, b, false, 2).unwrap_err();
    assert!(
        matches!(&error, ImageError::Io { path, .. } if path == &victim),
        "{error:?}"
    );
    assert!(error.to_string().contains("i/o error"));

    // The final file name is taken by a folder: the rename fails and the temp file is removed.
    let c = cache(CACHE_LIMIT_BYTES);
    std::fs::create_dir_all(c.dir.join(cache_key(a))).unwrap();
    serve_image(&http, a, &image(10, 3));
    let error = c.images.load(&c.store, &http, a, false, 1).unwrap_err();
    assert!(
        matches!(&error, ImageError::Io { path, .. } if path == &c.dir.join(cache_key(a))),
        "{error:?}"
    );
    assert!(!c.dir.join(format!("{}.tmp", cache_key(a))).exists());
    assert_eq!(rows(&c.db), 0);

    // The temp file name is taken by a folder.
    let c = cache(CACHE_LIMIT_BYTES);
    std::fs::create_dir_all(c.dir.join(format!("{}.tmp", cache_key(a)))).unwrap();
    serve_image(&http, a, &image(10, 3));
    let error = c.images.load(&c.store, &http, a, false, 1).unwrap_err();
    assert!(matches!(error, ImageError::Io { .. }), "{error:?}");

    // The cache folder cannot be created.
    let c = cache(CACHE_LIMIT_BYTES);
    let blocker = c._tmp.path().join("blocker");
    std::fs::write(&blocker, "file").unwrap();
    let images = RemoteImages::new(blocker.join("images"));
    let shown = RemoteImage {
        mime: "image/png".to_owned(),
        bytes: image(5, 1),
        from_cache: false,
    };
    let error = images.insert(&c.store, a, &shown, 1).unwrap_err();
    assert!(
        matches!(&error, ImageError::Io { path, .. } if path == &blocker.join("images")),
        "{error:?}"
    );
}

#[test]
fn image_errors_describe_themselves() {
    assert!(
        ImageError::Net(NetError::Timeout)
            .to_string()
            .contains("timed out")
    );
    assert!(
        ImageError::Store(StoreError::Cancelled)
            .to_string()
            .contains("cancelled")
    );
}

#[test]
fn images_download_and_cache_over_real_http() {
    let server = Server::start(|seen| match seen.target.as_str() {
        "/logo.png" => Reply::Bytes(image_response(&[9u8; 3000])),
        _ => Reply::Bytes(response(
            "200 OK",
            &[("Content-Type", "text/html"), ("Content-Length", "2")],
            b"hi",
        )),
    });
    let c = cache(CACHE_LIMIT_BYTES);
    let url = server.url("/logo.png");
    let first = loaded(c.images.load(&c.store, &WinHttp, &url, true, 1));
    assert_eq!((first.bytes.len(), first.from_cache), (3000, false));
    let second = loaded(c.images.load(&c.store, &WinHttp, &url, true, 2));
    assert!(second.from_cache);
    assert_eq!(server.requests().len(), 1);
    // Without the explicit allowance plain http is blocked even though a server is listening.
    assert_eq!(
        c.images
            .load(&c.store, &WinHttp, &server.url("/other.png"), false, 3)
            .unwrap(),
        ImageLoad::Blocked
    );
    // A page served as an image is refused.
    let error = c
        .images
        .load(&c.store, &WinHttp, &server.url("/page"), true, 4)
        .unwrap_err();
    assert!(
        matches!(error, ImageError::Net(NetError::WrongContentType(_))),
        "{error:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// Database failures of the image cache and of the update record
// ---------------------------------------------------------------------------------------------

/// Runs `sql` on a second connection to the database of `c`.
fn tamper(c: &Cache, sql: &str) {
    Connection::open(&c.db).unwrap().execute_batch(sql).unwrap();
}

/// The statement that makes every `event` (`INSERT`, `UPDATE`, `DELETE`) on `table` abort.
fn abort_on(event: &str, table: &str) -> String {
    format!(
        "CREATE TRIGGER abort_{event} BEFORE {event} ON {table}
         BEGIN SELECT RAISE(ABORT, 'injected failure'); END"
    )
}

fn shown(len: usize) -> RemoteImage {
    RemoteImage {
        mime: "image/png".to_owned(),
        bytes: image(len, 1),
        from_cache: false,
    }
}

/// The message of the database error `result` failed with.
fn database_error<T: std::fmt::Debug>(result: Result<T, ImageError>) -> String {
    match result.unwrap_err() {
        ImageError::Store(StoreError::Sqlite(error)) => error.to_string(),
        other => panic!("not a database error: {other:?}"),
    }
}

#[test]
fn cache_lookups_report_database_failures() {
    let url = "https://img.example.com/a.png";
    let http = ScriptedHttp::new();

    // The metadata table is gone: lookups and loads fail before anything is downloaded.
    let c = cache(CACHE_LIMIT_BYTES);
    tamper(&c, "DROP TABLE image_cache");
    let text = database_error(c.images.lookup(&c.store, url, 1));
    assert!(text.contains("no such table"), "{text}");
    let text = database_error(c.images.load(&c.store, &http, url, false, 1));
    assert!(text.contains("no such table"), "{text}");
    assert!(http.requests().is_empty());

    // A hit whose last-access time cannot be recorded.
    let c = cache(CACHE_LIMIT_BYTES);
    c.images.insert(&c.store, url, &shown(10), 1).unwrap();
    tamper(&c, &abort_on("UPDATE", "image_cache"));
    let text = database_error(c.images.lookup(&c.store, url, 2));
    assert!(text.contains("injected failure"), "{text}");

    // A damaged entry (its file is gone) that cannot be dropped.
    let c = cache(CACHE_LIMIT_BYTES);
    c.images.insert(&c.store, url, &shown(10), 1).unwrap();
    std::fs::remove_file(c.dir.join(cache_key(url))).unwrap();
    tamper(&c, &abort_on("DELETE", "image_cache"));
    let text = database_error(c.images.lookup(&c.store, url, 2));
    assert!(text.contains("injected failure"), "{text}");
    assert_eq!(rows(&c.db), 1);
}

#[test]
fn cache_inserts_report_database_failures() {
    let url = "https://img.example.com/a.png";
    let http = ScriptedHttp::new();
    let raw_entry = |key: &str, size: &str| {
        format!(
            "INSERT INTO image_cache (key, url, mime, size, last_access)
             VALUES ({key}, 'u', 'image/png', {size}, 1)"
        )
    };

    // The new entry cannot be recorded.
    let c = cache(CACHE_LIMIT_BYTES);
    tamper(&c, &abort_on("INSERT", "image_cache"));
    let text = database_error(c.images.insert(&c.store, url, &shown(10), 1));
    assert!(text.contains("injected failure"), "{text}");

    // The cache size cannot be added up: an entry has a size that is not a whole number.
    let c = cache(CACHE_LIMIT_BYTES);
    tamper(&c, &raw_entry("'odd'", "1.5"));
    let text = database_error(c.images.insert(&c.store, url, &shown(10), 2));
    assert!(text.contains("Invalid column type"), "{text}");

    // The entries cannot be listed for eviction: an entry has a key that is not text.
    let c = cache(10);
    tamper(&c, &raw_entry("x'6b6579'", "1000"));
    let text = database_error(c.images.insert(&c.store, url, &shown(10), 2));
    assert!(text.contains("Invalid column type"), "{text}");

    // An evicted entry cannot be deleted from the table.
    let (a, b) = (url, "https://img.example.com/b.png");
    let c = cache(150);
    serve_image(&http, a, &image(100, 1));
    serve_image(&http, b, &image(100, 2));
    loaded(c.images.load(&c.store, &http, a, false, 1));
    tamper(&c, &abort_on("DELETE", "image_cache"));
    let text = database_error(c.images.load(&c.store, &http, b, false, 2));
    assert!(text.contains("injected failure"), "{text}");
}

#[test]
fn invalid_urls_and_failed_requests_fail_a_download_and_an_insert() {
    let url = "https://img.example.com/a.png";
    let c = cache(CACHE_LIMIT_BYTES);
    let http = ScriptedHttp::new();
    assert!(matches!(
        c.images.download(&http, "not a url", false),
        Err(ImageError::Net(NetError::InvalidUrl(_)))
    ));
    http.respond(
        url,
        &head("404 Not Found", &[("Content-Type", "image/png")]),
        b"",
    );
    assert!(matches!(
        c.images.download(&http, url, false),
        Err(ImageError::Net(NetError::HttpStatus(404)))
    ));
    assert!(matches!(
        c.images.insert(&c.store, "not a url", &shown(1), 1),
        Err(ImageError::Net(NetError::InvalidUrl(_)))
    ));
}

#[test]
fn the_update_record_reports_database_failures() {
    let http = ScriptedHttp::new();
    let current = version("0.1.0");

    // The record is unreadable: the check stops before it asks the feed.
    let (tmp, store) = store();
    Connection::open(tmp.path().join("state.db"))
        .unwrap()
        .execute_batch("DROP TABLE kv")
        .unwrap();
    assert!(matches!(last_check(&store), Err(StoreError::Sqlite(_))));
    let error = check_for_updates(&store, &http, RELEASES_URL, &current, true, NOW).unwrap_err();
    assert!(
        matches!(&error, UpdateError::Store(StoreError::Sqlite(_))),
        "{error:?}"
    );
    assert!(http.requests().is_empty());

    // The answer cannot be recorded: the check fails and the next start asks again.
    let (tmp, store) = crate::store();
    Connection::open(tmp.path().join("state.db"))
        .unwrap()
        .execute_batch(&abort_on("INSERT", "kv"))
        .unwrap();
    http.respond(
        RELEASES_URL,
        &json_head(),
        release_json("v0.2.0", PAGE).as_bytes(),
    );
    let error = check_for_updates(&store, &http, RELEASES_URL, &current, true, NOW).unwrap_err();
    assert!(
        matches!(&error, UpdateError::Store(StoreError::Sqlite(_))),
        "{error:?}"
    );
    assert!(error.to_string().contains("injected failure"), "{error}");
    assert_eq!(last_check(&store).unwrap(), None);
}
