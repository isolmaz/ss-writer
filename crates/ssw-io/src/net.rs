//! HTTP GET for the few things the editor fetches (remote images, the update check, the
//! installer of an update).
//!
//! * [`Http`] is the network seam (architecture "OS seams"): one GET, no redirect following, with
//!   the response body bounded by a size cap and a deadline, collected in memory
//!   ([`Http::get`]) or handed to the caller chunk by chunk ([`Http::get_streaming`]).
//!   [`WinHttp`] is the real implementation over WinHTTP (no cookies, no authentication, no
//!   automatic redirects, system proxy settings except for loopback hosts); [`ScriptedHttp`]
//!   serves scripted raw responses and failures to tests and exercises the same response parsing
//!   and body limits.
//! * [`fetch`] is the policy on top: `https` only (`http` only when the caller allows it
//!   explicitly), at most [`MAX_REDIRECTS`] redirects (every hop is re-checked), status 200,
//!   acceptable content type, size cap, one overall deadline. [`download`] applies the same
//!   policy and streams the body into a file instead of memory.

use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::fmt;
use std::fs;
use std::io::Write;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use thiserror::Error;
use windows::Win32::Networking::WinHttp::{
    ERROR_WINHTTP_TIMEOUT, WINHTTP_ACCESS_TYPE, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
    WINHTTP_ACCESS_TYPE_NO_PROXY, WINHTTP_DISABLE_AUTHENTICATION, WINHTTP_DISABLE_COOKIES,
    WINHTTP_DISABLE_REDIRECTS, WINHTTP_FLAG_SECURE, WINHTTP_OPEN_REQUEST_FLAGS,
    WINHTTP_OPTION_DISABLE_FEATURE, WINHTTP_OPTION_RECEIVE_RESPONSE_TIMEOUT,
    WINHTTP_QUERY_RAW_HEADERS_CRLF, WinHttpCloseHandle, WinHttpConnect, WinHttpOpen,
    WinHttpOpenRequest, WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse,
    WinHttpSendRequest, WinHttpSetOption, WinHttpSetTimeouts,
};
use windows::core::PCWSTR;

/// Redirects followed by [`fetch`] before it gives up.
pub const MAX_REDIRECTS: usize = 5;

/// `User-Agent` of every request.
pub const USER_AGENT: &str = concat!("SS Writer/", env!("CARGO_PKG_VERSION"));

/// Body bytes requested from the network per read.
const READ_CHUNK: usize = 16 * 1024;

/// Chunks [`WinHttp::get_streaming`]'s worker may be ahead of the caller; a slower consumer (a
/// disk) holds the network back instead of filling the memory.
const STREAM_BACKLOG: usize = 8;

/// Capacity (UTF-16 units) of the buffer receiving the raw response headers; WinHTTP rejects
/// responses whose headers exceed 64 KiB, which always fits.
const HEADER_BUFFER_UNITS: usize = 64 * 1024;

/// Everything that can go wrong fetching a URL.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum NetError {
    #[error("invalid URL: {0}")]
    InvalidUrl(&'static str),
    #[error("unsupported URL scheme `{0}`")]
    UnsupportedScheme(String),
    #[error("plain http is not allowed")]
    InsecureScheme,
    #[error("more than {MAX_REDIRECTS} redirects")]
    TooManyRedirects,
    #[error("redirect without a usable Location header")]
    BadRedirect,
    #[error("server answered with status {0}")]
    HttpStatus(u16),
    #[error("unexpected content type {0:?}")]
    WrongContentType(Option<String>),
    #[error("response is larger than {limit} bytes")]
    TooLarge { limit: u64 },
    #[error("the request timed out")]
    Timeout,
    #[error("malformed response: {0}")]
    MalformedResponse(&'static str),
    #[error("{call} failed with error {code}")]
    Os { call: &'static str, code: u32 },
}

/// URL scheme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scheme {
    Http,
    Https,
}

impl Scheme {
    const fn default_port(self) -> u16 {
        match self {
            Scheme::Http => 80,
            Scheme::Https => 443,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Scheme::Http => "http",
            Scheme::Https => "https",
        }
    }
}

/// An absolute `http` / `https` URL without credentials and fragment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Url {
    scheme: Scheme,
    /// Host name or IP literal (IPv6 without brackets).
    host: String,
    port: u16,
    /// Path and query, always starting with `/`, with unsafe bytes percent-encoded.
    target: String,
}

/// Percent-encode what may not appear in a request target: controls, space, non-ASCII and the
/// characters RFC 3986 excludes. Existing `%XX` escapes are kept.
fn encode_target(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        if !(0x21..0x7F).contains(&byte) || b"\"<>\\^`{|}".contains(&byte) {
            out.push_str(&format!("%{byte:02X}"));
        } else {
            out.push(char::from(byte));
        }
    }
    out
}

/// `path` with `.` and `..` segments resolved (the query, if any, is kept as is).
fn remove_dot_segments(target: &str) -> String {
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (target, None),
    };
    let mut segments: Vec<&str> = Vec::new();
    let mut parts = path.split('/').peekable();
    while let Some(part) = parts.next() {
        match part {
            "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
        if parts.peek().is_none() && matches!(part, "." | "..") {
            segments.push("");
        }
    }
    let mut out = segments.join("/");
    if !out.starts_with('/') {
        out.insert(0, '/');
    }
    if let Some(query) = query {
        out.push('?');
        out.push_str(query);
    }
    out
}

fn is_scheme_prefix(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
}

impl Url {
    /// Parse an absolute `http` / `https` URL. The fragment is dropped; credentials are
    /// rejected; the host must be ASCII (no IDNA conversion is done here).
    ///
    /// # Errors
    /// [`NetError::InvalidUrl`] and [`NetError::UnsupportedScheme`].
    pub fn parse(text: &str) -> Result<Self, NetError> {
        let text = text.trim();
        let (scheme_text, rest) = text
            .split_once("://")
            .ok_or(NetError::InvalidUrl("missing scheme"))?;
        let scheme = match scheme_text.to_ascii_lowercase().as_str() {
            "https" => Scheme::Https,
            "http" => Scheme::Http,
            other => return Err(NetError::UnsupportedScheme(other.to_owned())),
        };
        let rest = rest.split('#').next().unwrap_or(rest);
        let (authority, target) = match rest.find(['/', '?']) {
            Some(at) => rest.split_at(at),
            None => (rest, "/"),
        };
        if authority.contains('@') {
            return Err(NetError::InvalidUrl("credentials are not allowed"));
        }
        let (host, port_text) = if let Some(bracketed) = authority.strip_prefix('[') {
            let (host, after) = bracketed
                .split_once(']')
                .ok_or(NetError::InvalidUrl("unterminated IPv6 literal"))?;
            if host.parse::<std::net::Ipv6Addr>().is_err() {
                return Err(NetError::InvalidUrl("invalid IPv6 literal"));
            }
            let port = match after {
                "" => None,
                _ => Some(
                    after
                        .strip_prefix(':')
                        .ok_or(NetError::InvalidUrl("invalid port"))?,
                ),
            };
            (host, port)
        } else {
            let (host, port) = match authority.rsplit_once(':') {
                Some((host, port)) => (host, Some(port)),
                None => (authority, None),
            };
            let valid_host = !host.is_empty()
                && host
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'));
            if !valid_host {
                return Err(NetError::InvalidUrl("invalid host"));
            }
            (host, port)
        };
        let port = match port_text {
            None => scheme.default_port(),
            Some(port) => match port.parse::<u16>() {
                Ok(port) if port != 0 => port,
                _ => return Err(NetError::InvalidUrl("invalid port")),
            },
        };
        let target = if target.starts_with('/') {
            encode_target(target)
        } else {
            encode_target(&format!("/{target}"))
        };
        Ok(Self {
            scheme,
            host: host.to_ascii_lowercase(),
            port,
            target,
        })
    }

    /// Resolve a redirect `Location` against this URL: absolute URLs, scheme-relative
    /// (`//host/x`), absolute paths, query-only and relative references.
    ///
    /// # Errors
    /// [`NetError::BadRedirect`] for an empty location and the errors of [`Url::parse`].
    pub fn join(&self, location: &str) -> Result<Self, NetError> {
        let location = location.trim();
        let location = location.split('#').next().unwrap_or(location);
        if location.is_empty() {
            return Err(NetError::BadRedirect);
        }
        if location
            .split_once("://")
            .is_some_and(|(scheme, _)| is_scheme_prefix(scheme))
        {
            return Self::parse(location);
        }
        if location.starts_with("//") {
            return Self::parse(&format!("{}:{location}", self.scheme.name()));
        }
        let target = if location.starts_with('/') {
            remove_dot_segments(location)
        } else if location.starts_with('?') {
            let path = self.target.split('?').next().unwrap_or(&self.target);
            format!("{path}{location}")
        } else {
            let path = self.target.split('?').next().unwrap_or(&self.target);
            let directory = &path[..=path.rfind('/').unwrap_or(0)];
            remove_dot_segments(&format!("{directory}{location}"))
        };
        Ok(Self {
            target: encode_target(&target),
            ..self.clone()
        })
    }

    #[must_use]
    pub fn scheme(&self) -> Scheme {
        self.scheme
    }

    /// Host name or IP literal (IPv6 without brackets).
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Path and query as sent in the request line.
    #[must_use]
    pub fn target(&self) -> &str {
        &self.target
    }
}

impl fmt::Display for Url {
    /// The normalized text: lower-case scheme and host, default port omitted, no fragment.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}://", self.scheme.name())?;
        if self.host.contains(':') {
            write!(f, "[{}]", self.host)?;
        } else {
            f.write_str(&self.host)?;
        }
        if self.port != self.scheme.default_port() {
            write!(f, ":{}", self.port)?;
        }
        f.write_str(&self.target)
    }
}

/// What the caller wants back; decides the `Accept` header and which responses get a body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Accept {
    /// Any content type.
    Any,
    /// `image/*`.
    Image,
    /// `application/json` and `+json` types.
    Json,
}

impl Accept {
    /// The `Accept` request header value.
    #[must_use]
    pub const fn header_value(self) -> &'static str {
        match self {
            Accept::Any => "*/*",
            Accept::Image => "image/*",
            Accept::Json => "application/json",
        }
    }

    /// Whether a response of the (lower-case, parameter-free) content type `mime` is acceptable.
    #[must_use]
    pub fn allows(self, mime: Option<&str>) -> bool {
        match self {
            Accept::Any => true,
            Accept::Image => mime.is_some_and(|m| m.starts_with("image/")),
            Accept::Json => mime.is_some_and(|m| m == "application/json" || m.ends_with("+json")),
        }
    }
}

/// Status line and headers of a response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResponseHead {
    pub status: u16,
    /// Header names lower-cased, values trimmed, in order of appearance.
    pub headers: Vec<(String, String)>,
}

impl ResponseHead {
    /// Parse the raw header block (`HTTP/1.1 200 OK` followed by `Name: value` lines).
    ///
    /// # Errors
    /// [`NetError::MalformedResponse`] for a missing or invalid status line or a header line
    /// without a colon.
    pub fn parse(raw: &str) -> Result<Self, NetError> {
        let mut lines = raw.lines();
        let mut status_line = lines.next().unwrap_or_default().split_whitespace();
        if !status_line.next().is_some_and(|v| v.starts_with("HTTP/")) {
            return Err(NetError::MalformedResponse("missing status line"));
        }
        let status = status_line
            .next()
            .and_then(|code| code.parse::<u16>().ok())
            .filter(|code| (100..=599).contains(code))
            .ok_or(NetError::MalformedResponse("invalid status code"))?;
        let mut headers = Vec::new();
        for line in lines.take_while(|line| !line.is_empty()) {
            let (name, value) = line
                .split_once(':')
                .ok_or(NetError::MalformedResponse("header line without a colon"))?;
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
        Ok(Self { status, headers })
    }

    /// First value of the header `name` (case-insensitive).
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// `Content-Type` without parameters, lower-cased.
    #[must_use]
    pub fn mime(&self) -> Option<String> {
        let value = self.header("content-type")?;
        let mime = value.split(';').next().unwrap_or(value).trim();
        (!mime.is_empty()).then(|| mime.to_ascii_lowercase())
    }

    /// `Content-Length`, when present and numeric.
    #[must_use]
    pub fn content_length(&self) -> Option<u64> {
        self.header("content-length")?.parse().ok()
    }
}

/// One GET without redirect following.
#[derive(Clone, Copy, Debug)]
pub struct HttpRequest<'a> {
    pub url: &'a Url,
    /// Deadline for the whole exchange, response body included.
    pub timeout: Duration,
    /// Largest body accepted.
    pub max_body: u64,
    pub accept: Accept,
}

/// A response: the head always, the body only for status 200 with an acceptable content type
/// (for everything else it is empty and never downloaded).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpResponse {
    pub head: ResponseHead,
    pub body: Vec<u8>,
}

/// The consumer of a streamed body ([`Http::get_streaming`]): takes every chunk together with the
/// size the server announced for the whole body, when it did, and may fail to stop the stream.
pub type BodySink<'a> = dyn FnMut(&[u8], Option<u64>) -> Result<(), NetError> + 'a;

/// The network seam.
pub trait Http: Send + Sync + fmt::Debug {
    /// Perform the GET. Redirects are returned, not followed.
    ///
    /// # Errors
    /// [`NetError::Timeout`] when the deadline passes, [`NetError::TooLarge`] when the body
    /// exceeds `max_body` (also decided from `Content-Length` before downloading),
    /// [`NetError::MalformedResponse`] and [`NetError::Os`].
    fn get(&self, request: &HttpRequest<'_>) -> Result<HttpResponse, NetError>;

    /// Perform the GET like [`Http::get`], but hand every body chunk to `sink` instead of
    /// collecting it. `sink` also receives the size the server announced (`Content-Length`),
    /// when it sent one, with every chunk; it is not called at all for a response whose body is
    /// not wanted (see [`HttpResponse`]). The head is returned once the body is complete. An
    /// error of `sink` ends the exchange and is returned as it is.
    ///
    /// The default replays the body [`Http::get`] collected, in pieces, which is right for test
    /// doubles but holds the whole body in memory; [`WinHttp`] and [`ScriptedHttp`] stream for
    /// real.
    ///
    /// # Errors
    /// Those of [`Http::get`] (the size cap is checked against the bytes handed to `sink`) and
    /// whatever `sink` returns.
    fn get_streaming(
        &self,
        request: &HttpRequest<'_>,
        sink: &mut BodySink<'_>,
    ) -> Result<ResponseHead, NetError> {
        let HttpResponse { head, body } = self.get(request)?;
        let total = head.content_length();
        for chunk in body.chunks(READ_CHUNK) {
            sink(chunk, total)?;
        }
        Ok(head)
    }
}

/// Whether the body of a response with this head is downloaded; fails early when the announced
/// size is over the limit.
fn body_wanted(head: &ResponseHead, request: &HttpRequest<'_>) -> Result<bool, NetError> {
    if head.status != 200 || !request.accept.allows(head.mime().as_deref()) {
        return Ok(false);
    }
    if head
        .content_length()
        .is_some_and(|length| length > request.max_body)
    {
        return Err(NetError::TooLarge {
            limit: request.max_body,
        });
    }
    Ok(true)
}

/// Parse the raw header block `raw_head`, decide whether the body is wanted and hand it from
/// `read` (fills the buffer, returns the byte count, 0 at the end) to `sink` chunk by chunk
/// under the size cap and the deadline of `request`, which started at `started`. Shared by every
/// [`Http`] implementation, so they all answer the same way for the same bytes.
fn complete_streaming(
    raw_head: &str,
    request: &HttpRequest<'_>,
    started: Instant,
    mut read: impl FnMut(&mut [u8]) -> Result<usize, NetError>,
    sink: &mut BodySink<'_>,
) -> Result<ResponseHead, NetError> {
    let head = ResponseHead::parse(raw_head)?;
    if !body_wanted(&head, request)? {
        return Ok(head);
    }
    let announced = head.content_length();
    let mut received = 0u64;
    let mut chunk = vec![0u8; READ_CHUNK];
    loop {
        if started.elapsed() > request.timeout {
            return Err(NetError::Timeout);
        }
        let count = read(&mut chunk)?;
        if count == 0 {
            break;
        }
        received += count as u64;
        if received > request.max_body {
            return Err(NetError::TooLarge {
                limit: request.max_body,
            });
        }
        sink(&chunk[..count], announced)?;
    }
    // A connection that dropped early must not yield a silently truncated image.
    if announced.is_some_and(|length| length != received) {
        return Err(NetError::MalformedResponse(
            "body does not match Content-Length",
        ));
    }
    Ok(head)
}

/// Like [`complete_streaming`], collecting the body in memory.
fn complete(
    raw_head: &str,
    request: &HttpRequest<'_>,
    started: Instant,
    read: impl FnMut(&mut [u8]) -> Result<usize, NetError>,
) -> Result<HttpResponse, NetError> {
    let mut body = Vec::new();
    let head = complete_streaming(raw_head, request, started, read, &mut |chunk, _| {
        body.extend_from_slice(chunk);
        Ok(())
    })?;
    Ok(HttpResponse { head, body })
}

// ---------------------------------------------------------------------------------------------
// Policy
// ---------------------------------------------------------------------------------------------

/// Limits and rules for [`fetch`] and [`download`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FetchOptions {
    pub accept: Accept,
    pub max_body: u64,
    /// Deadline for the whole fetch, redirects included.
    pub timeout: Duration,
    /// Permit `http` URLs (and downgrades by redirect). Off unless the user explicitly asked.
    pub allow_http: bool,
}

/// A successful fetch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fetched {
    /// The URL that finally answered (after redirects).
    pub url: Url,
    /// Content type without parameters, lower-cased.
    pub mime: Option<String>,
    pub body: Vec<u8>,
}

/// GET `url` through `http` under `options`.
///
/// # Errors
/// [`NetError::InsecureScheme`] for `http` without `allow_http` (checked again for every
/// redirect hop), [`NetError::TooManyRedirects`], [`NetError::BadRedirect`],
/// [`NetError::HttpStatus`] for anything but 200 / redirects, [`NetError::WrongContentType`],
/// [`NetError::Timeout`] when the deadline passes between hops, and whatever `http` reports.
pub fn fetch(http: &dyn Http, url: &str, options: &FetchOptions) -> Result<Fetched, NetError> {
    let (url, head, body) = follow(url, options, |request| {
        let response = http.get(request)?;
        Ok((response.head, response.body))
    })?;
    Ok(Fetched {
        url,
        mime: head.mime(),
        body,
    })
}

/// The redirect policy shared by [`fetch`] and [`download`]: `get` performs one GET and returns
/// its head together with what the caller took from that hop (the body, or the file being
/// written). Returns the URL that answered with status 200 and an acceptable content type, with
/// its head and what `get` returned for it.
fn follow<T>(
    url: &str,
    options: &FetchOptions,
    mut get: impl FnMut(&HttpRequest<'_>) -> Result<(ResponseHead, T), NetError>,
) -> Result<(Url, ResponseHead, T), NetError> {
    let started = Instant::now();
    let mut current = Url::parse(url)?;
    let mut redirects = 0;
    loop {
        if current.scheme() == Scheme::Http && !options.allow_http {
            return Err(NetError::InsecureScheme);
        }
        let remaining = options
            .timeout
            .checked_sub(started.elapsed())
            .filter(|remaining| !remaining.is_zero())
            .ok_or(NetError::Timeout)?;
        let (head, taken) = get(&HttpRequest {
            url: &current,
            timeout: remaining,
            max_body: options.max_body,
            accept: options.accept,
        })?;
        match head.status {
            200 => {
                let mime = head.mime();
                if !options.accept.allows(mime.as_deref()) {
                    return Err(NetError::WrongContentType(mime));
                }
                return Ok((current, head, taken));
            }
            301 | 302 | 303 | 307 | 308 => {
                if redirects == MAX_REDIRECTS {
                    return Err(NetError::TooManyRedirects);
                }
                redirects += 1;
                let location = head.header("location").ok_or(NetError::BadRedirect)?;
                current = current.join(location)?;
            }
            other => return Err(NetError::HttpStatus(other)),
        }
    }
}

/// A finished [`download`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Downloaded {
    /// The URL that finally answered (after redirects).
    pub url: Url,
    /// Bytes written to the file.
    pub bytes: u64,
    /// Lower-case hex SHA-256 of those bytes.
    pub sha256: String,
}

/// A file that is deleted when dropped, unless `keep` is set (it was moved to its final name).
struct Scratch {
    path: PathBuf,
    keep: bool,
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if !self.keep {
            // Best effort: whatever made the download fail is the error worth reporting.
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// A download in progress: the temporary file next to the target with the digest and the byte
/// count of what was written to it. Dropping it deletes the file.
struct Partial {
    // Declared before `scratch`: the handle is closed before the file is deleted.
    file: fs::File,
    scratch: Scratch,
    hasher: Sha256,
    bytes: u64,
}

/// `error` as the `call` that failed, in the vocabulary of [`NetError::Os`].
fn io_failure(call: &'static str, error: &std::io::Error) -> NetError {
    NetError::Os {
        call,
        code: error.raw_os_error().map_or(0, i32::cast_unsigned),
    }
}

/// Lower-case hex text of `digest`.
fn hex(digest: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(digest.len() * 2);
    for byte in digest {
        text.push(char::from(DIGITS[usize::from(byte >> 4)]));
        text.push(char::from(DIGITS[usize::from(byte & 0xF)]));
    }
    text
}

impl Partial {
    /// Create (or empty) the temporary file of `target`.
    fn create(target: &Path) -> Result<Self, NetError> {
        let mut name = target.as_os_str().to_owned();
        name.push(".part");
        let path = PathBuf::from(name);
        let file = fs::File::create(&path).map_err(|error| io_failure("CreateFileW", &error))?;
        Ok(Self {
            file,
            scratch: Scratch { path, keep: false },
            hasher: Sha256::new(),
            bytes: 0,
        })
    }

    fn write(&mut self, chunk: &[u8]) -> Result<(), NetError> {
        self.file
            .write_all(chunk)
            .map_err(|error| io_failure("WriteFile", &error))?;
        self.hasher.update(chunk);
        self.bytes += chunk.len() as u64;
        Ok(())
    }

    /// Move the file to `target`, replacing what is there.
    fn finish(self, target: &Path, url: Url) -> Result<Downloaded, NetError> {
        let Self {
            file,
            mut scratch,
            hasher,
            bytes,
        } = self;
        drop(file);
        fs::rename(&scratch.path, target).map_err(|error| io_failure("MoveFileExW", &error))?;
        scratch.keep = true;
        Ok(Downloaded {
            url,
            bytes,
            sha256: hex(&hasher.finalize()),
        })
    }
}

/// GET `url` through `http` under `options` like [`fetch`], but stream the body into the file
/// `path` and report `(downloaded, total)` to `progress` after every chunk (`total` is the
/// size the server announced, when it did). The bytes go to a temporary file next to `path`
/// (`<path>.part`) that replaces `path` only once the body is complete, so a failed download
/// leaves neither a half-written file nor a damaged earlier one behind. Nothing is created for
/// a response that is not a 200 with an acceptable content type.
///
/// # Errors
/// The errors of [`fetch`] ([`NetError::TooLarge`] is decided from `Content-Length` before the
/// first byte and again while the body arrives), and [`NetError::Os`] when the file cannot be
/// created, written or moved into place.
pub fn download(
    http: &dyn Http,
    url: &str,
    options: &FetchOptions,
    path: &Path,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<Downloaded, NetError> {
    let (url, _, partial) = follow(url, options, |request| {
        let mut partial = None;
        let head = http.get_streaming(request, &mut |chunk, total| {
            let open = match &mut partial {
                Some(open) => open,
                none => none.insert(Partial::create(path)?),
            };
            open.write(chunk)?;
            progress(open.bytes, total);
            Ok(())
        })?;
        Ok((head, partial))
    })?;
    // A body of zero bytes never reached the sink.
    let partial = match partial {
        Some(partial) => partial,
        None => Partial::create(path)?,
    };
    partial.finish(path, url)
}

// ---------------------------------------------------------------------------------------------
// WinHTTP
// ---------------------------------------------------------------------------------------------

/// The real network: WinHTTP. Cookies, authentication and automatic redirects are disabled;
/// the system proxy configuration is used except for loopback hosts.
#[derive(Clone, Copy, Debug, Default)]
pub struct WinHttp;

/// An open WinHTTP handle (session, connection or request), closed on drop.
struct Handle(*mut c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle came from a successful WinHttp open call and is closed once, here.
        // A failure to close leaves nothing to recover.
        let _ = unsafe { WinHttpCloseHandle(self.0) };
    }
}

fn os_error(call: &'static str, code: u32) -> NetError {
    if code == ERROR_WINHTTP_TIMEOUT {
        NetError::Timeout
    } else {
        NetError::Os { call, code }
    }
}

/// The handle `raw` returned by an open call, or the thread's last error when it is null.
fn handle(raw: *mut c_void) -> windows::core::Result<Handle> {
    if raw.is_null() {
        Err(windows::core::Error::from_thread())
    } else {
        Ok(Handle(raw))
    }
}

/// The raw handle of a step that has already run (null before that; the steps run in order and
/// stop at the first failure, so a step never sees a handle it needs missing).
fn raw_of(handle: &Option<Handle>) -> *mut c_void {
    handle.as_ref().map_or(std::ptr::null_mut(), |open| open.0)
}

fn check<T>(call: &'static str, result: windows::core::Result<T>) -> Result<T, NetError> {
    // The windows crate reports a failed BOOL as HRESULT_FROM_WIN32(GetLastError()): the Win32
    // code is the low word.
    result.map_err(|error| os_error(call, error.code().0.cast_unsigned() & 0xFFFF))
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Loopback hosts are reached directly; everything else follows the system proxy settings.
fn access_type(host: &str) -> WINHTTP_ACCESS_TYPE {
    if is_loopback(host) {
        WINHTTP_ACCESS_TYPE_NO_PROXY
    } else {
        WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY
    }
}

/// The state of one request while its WinHTTP calls run in order.
struct Exchange<'a> {
    request: HttpRequest<'a>,
    // Declaration order is closing order: request, connection, session.
    http_request: Option<Handle>,
    connection: Option<Handle>,
    session: Option<Handle>,
    /// The raw header block, filled by the last step.
    raw_head: String,
}

impl Exchange<'_> {
    /// The deadline as the millisecond value WinHTTP takes (at least 1, since 0 means "never").
    fn timeout_ms(&self) -> i32 {
        i32::try_from(self.request.timeout.as_millis())
            .unwrap_or(i32::MAX)
            .max(1)
    }

    /// One `WinHttpReadData`: fills `buffer`, returns the byte count (0 at the end).
    fn read(&self, buffer: &mut [u8]) -> Result<usize, NetError> {
        let mut read = 0u32;
        // SAFETY: the request handle is open (all steps ran); `buffer` is writable for its
        // length (at most `READ_CHUNK`, which fits a u32) and `read` outlives the call.
        let result = unsafe {
            WinHttpReadData(
                raw_of(&self.http_request),
                buffer.as_mut_ptr().cast(),
                buffer.len() as u32,
                &mut read,
            )
        };
        check("WinHttpReadData", result).map(|()| read as usize)
    }
}

type Step = (
    &'static str,
    fn(&mut Exchange<'_>) -> windows::core::Result<()>,
);

/// The WinHTTP calls that send one GET and receive its head, in order. The first failure ends
/// the exchange, so a step may rely on the handles of the steps before it.
const STEPS: [Step; 9] = [
    ("WinHttpOpen", open_session),
    ("WinHttpSetTimeouts", set_timeouts),
    ("WinHttpConnect", connect),
    ("WinHttpOpenRequest", open_request),
    ("WinHttpSetOption", disable_features),
    ("WinHttpSetOption", limit_response_wait),
    ("WinHttpSendRequest", send_request),
    ("WinHttpReceiveResponse", receive_response),
    ("WinHttpQueryHeaders", query_headers),
];

fn open_session(exchange: &mut Exchange<'_>) -> windows::core::Result<()> {
    let agent = wide(USER_AGENT);
    // SAFETY: `agent` is NUL-terminated and outlives the call; the proxy arguments are null as
    // required for these access types.
    let session = unsafe {
        WinHttpOpen(
            PCWSTR(agent.as_ptr()),
            access_type(exchange.request.url.host()),
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        )
    };
    handle(session).map(|open| exchange.session = Some(open))
}

fn set_timeouts(exchange: &mut Exchange<'_>) -> windows::core::Result<()> {
    let ms = exchange.timeout_ms();
    // SAFETY: the session handle is open.
    unsafe { WinHttpSetTimeouts(raw_of(&exchange.session), ms, ms, ms, ms) }
}

fn connect(exchange: &mut Exchange<'_>) -> windows::core::Result<()> {
    let host = wide(exchange.request.url.host());
    // SAFETY: the session handle is open and `host` is NUL-terminated and outlives the call.
    let connection = unsafe {
        WinHttpConnect(
            raw_of(&exchange.session),
            PCWSTR(host.as_ptr()),
            exchange.request.url.port(),
            0,
        )
    };
    handle(connection).map(|open| exchange.connection = Some(open))
}

fn open_request(exchange: &mut Exchange<'_>) -> windows::core::Result<()> {
    let verb = wide("GET");
    let target = wide(exchange.request.url.target());
    let flags = match exchange.request.url.scheme() {
        Scheme::Https => WINHTTP_FLAG_SECURE,
        Scheme::Http => WINHTTP_OPEN_REQUEST_FLAGS(0),
    };
    // SAFETY: the connection handle is open; the strings are NUL-terminated and outlive the
    // call; a null version selects HTTP/1.1, a null referrer and null accept types mean none.
    let request = unsafe {
        WinHttpOpenRequest(
            raw_of(&exchange.connection),
            PCWSTR(verb.as_ptr()),
            PCWSTR(target.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            flags,
        )
    };
    handle(request).map(|open| exchange.http_request = Some(open))
}

/// `WinHttpSetOption` with a DWORD value on the request handle.
fn set_option(exchange: &Exchange<'_>, option: u32, value: u32) -> windows::core::Result<()> {
    // SAFETY: the request handle is open and the value is passed as its 4 bytes.
    unsafe {
        WinHttpSetOption(
            Some(raw_of(&exchange.http_request).cast_const()),
            option,
            Some(&value.to_ne_bytes()[..]),
        )
    }
}

fn disable_features(exchange: &mut Exchange<'_>) -> windows::core::Result<()> {
    let disabled =
        WINHTTP_DISABLE_COOKIES | WINHTTP_DISABLE_REDIRECTS | WINHTTP_DISABLE_AUTHENTICATION;
    set_option(exchange, WINHTTP_OPTION_DISABLE_FEATURE, disabled)
}

/// `WinHttpSetTimeouts` does not cover the wait for the response head; this option does.
fn limit_response_wait(exchange: &mut Exchange<'_>) -> windows::core::Result<()> {
    let ms = exchange.timeout_ms().cast_unsigned();
    set_option(exchange, WINHTTP_OPTION_RECEIVE_RESPONSE_TIMEOUT, ms)
}

fn send_request(exchange: &mut Exchange<'_>) -> windows::core::Result<()> {
    let headers: Vec<u16> = format!("Accept: {}\r\n", exchange.request.accept.header_value())
        .encode_utf16()
        .collect();
    // SAFETY: the request handle is open; `headers` is passed with its length and outlives the
    // call; there is no request body.
    unsafe {
        WinHttpSendRequest(
            raw_of(&exchange.http_request),
            Some(headers.as_slice()),
            None,
            0,
            0,
            0,
        )
    }
}

fn receive_response(exchange: &mut Exchange<'_>) -> windows::core::Result<()> {
    // SAFETY: the request handle is open and its request was sent; the reserved argument is null.
    unsafe { WinHttpReceiveResponse(raw_of(&exchange.http_request), std::ptr::null_mut()) }
}

fn query_headers(exchange: &mut Exchange<'_>) -> windows::core::Result<()> {
    let mut buffer = vec![0u16; HEADER_BUFFER_UNITS];
    let mut bytes = (buffer.len() * 2) as u32;
    // SAFETY: the request handle is open and has a response; `buffer` holds `bytes` writable
    // bytes; the name and index arguments are unused for WINHTTP_QUERY_RAW_HEADERS_CRLF.
    let result = unsafe {
        WinHttpQueryHeaders(
            raw_of(&exchange.http_request),
            WINHTTP_QUERY_RAW_HEADERS_CRLF,
            PCWSTR::null(),
            Some(buffer.as_mut_ptr().cast()),
            &mut bytes,
            std::ptr::null_mut(),
        )
    };
    result.map(|()| {
        buffer.truncate(bytes as usize / 2);
        exchange.raw_head = String::from_utf16_lossy(&buffer)
            .trim_end_matches('\0')
            .to_owned();
    })
}

/// Send the GET and receive the head; the body is still unread. Returns the exchange and the
/// raw header block.
fn start_exchange<'a>(request: &HttpRequest<'a>) -> Result<(Exchange<'a>, String), NetError> {
    let mut exchange = Exchange {
        request: *request,
        http_request: None,
        connection: None,
        session: None,
        raw_head: String::new(),
    };
    for (call, step) in STEPS {
        check(call, step(&mut exchange))?;
    }
    let raw_head = std::mem::take(&mut exchange.raw_head);
    Ok((exchange, raw_head))
}

/// The whole exchange: send the GET, receive the head and the body.
fn run_exchange(request: &HttpRequest<'_>) -> Result<HttpResponse, NetError> {
    let started = Instant::now();
    let (exchange, raw_head) = start_exchange(request)?;
    complete(&raw_head, request, started, |buffer| exchange.read(buffer))
}

/// The whole exchange, the body going to `sink` chunk by chunk.
fn run_streaming_exchange(
    request: &HttpRequest<'_>,
    sink: &mut BodySink<'_>,
) -> Result<ResponseHead, NetError> {
    let started = Instant::now();
    let (exchange, raw_head) = start_exchange(request)?;
    complete_streaming(
        &raw_head,
        request,
        started,
        |buffer| exchange.read(buffer),
        sink,
    )
}

/// What a worker thread needs of an [`HttpRequest`]: a copy it owns.
struct OwnedRequest {
    url: Url,
    timeout: Duration,
    max_body: u64,
    accept: Accept,
}

impl OwnedRequest {
    fn of(request: &HttpRequest<'_>) -> Self {
        Self {
            url: request.url.clone(),
            timeout: request.timeout,
            max_body: request.max_body,
            accept: request.accept,
        }
    }

    fn request(&self) -> HttpRequest<'_> {
        HttpRequest {
            url: &self.url,
            timeout: self.timeout,
            max_body: self.max_body,
            accept: self.accept,
        }
    }
}

/// What the worker of [`WinHttp::get_streaming`] sends to the thread that called it.
enum Streamed {
    Chunk { data: Vec<u8>, total: Option<u64> },
    Finished(Result<ResponseHead, NetError>),
}

impl Http for WinHttp {
    /// WinHTTP's own time-outs do not bound every phase of a synchronous exchange (a server that
    /// accepts and then stays silent can hold `WinHttpReceiveResponse` or `WinHttpReadData` far
    /// past them), so the exchange runs on a worker thread and the deadline is enforced here.
    /// A worker still stuck when the deadline passes ends, and releases its handles, on its own.
    fn get(&self, request: &HttpRequest<'_>) -> Result<HttpResponse, NetError> {
        let (sender, receiver) = std::sync::mpsc::channel();
        let owned = OwnedRequest::of(request);
        std::thread::spawn(move || {
            // The receiver is gone when the deadline passed first; nobody wants the result then.
            let _ = sender.send(run_exchange(&owned.request()));
        });
        // No answer in time (or a worker that died without one) is a time-out.
        receiver
            .recv_timeout(request.timeout)
            .unwrap_or(Err(NetError::Timeout))
    }

    /// The worker of [`WinHttp::get`] again, but the body chunks cross the channel as they
    /// arrive and this thread feeds them to `sink`. Every wait for the next message is bounded
    /// by what is left of the deadline, which started with the call, so a body that stops
    /// arriving times out after the same time as a head that never comes. The channel holds a
    /// few chunks only, so a sink that is slower than the network holds the worker back.
    fn get_streaming(
        &self,
        request: &HttpRequest<'_>,
        sink: &mut BodySink<'_>,
    ) -> Result<ResponseHead, NetError> {
        let started = Instant::now();
        let (sender, receiver) = std::sync::mpsc::sync_channel(STREAM_BACKLOG);
        let owned = OwnedRequest::of(request);
        std::thread::spawn(move || {
            let outcome = run_streaming_exchange(&owned.request(), &mut |chunk, total| {
                // A closed channel means the caller stopped listening (deadline passed, or its
                // sink failed): that ends the exchange, the error itself is never read.
                sender
                    .send(Streamed::Chunk {
                        data: chunk.to_vec(),
                        total,
                    })
                    .map_err(|_| NetError::Timeout)
            });
            let _ = sender.send(Streamed::Finished(outcome));
        });
        loop {
            let wait = request.timeout.saturating_sub(started.elapsed());
            // No message in time (or a worker that died without a last one) is a time-out.
            match receiver.recv_timeout(wait) {
                Ok(Streamed::Chunk { data, total }) => sink(&data, total)?,
                Ok(Streamed::Finished(outcome)) => return outcome,
                Err(_) => return Err(NetError::Timeout),
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Scripted network for tests
// ---------------------------------------------------------------------------------------------

/// One scripted answer of a [`ScriptedHttp`] URL.
#[derive(Clone, Debug)]
enum Reply {
    Response {
        head: String,
        body: Vec<u8>,
        delay: Duration,
        /// The failure the connection ends with once `body` has been read.
        broken: Option<NetError>,
    },
    Failure(NetError),
}

#[derive(Debug, Default)]
struct Script {
    replies: HashMap<String, VecDeque<Reply>>,
    requests: Vec<String>,
}

/// A network that answers from a script. Each URL has a queue of replies consumed in order (a
/// URL with an empty queue fails like an unreachable host). Responses are given as raw header
/// text and body bytes and go through the same parsing, size cap and deadline code as
/// [`WinHttp`], whether collected by [`Http::get`] or streamed by [`Http::get_streaming`] (in
/// chunks of the size `WinHttp` reads). Every request URL is recorded.
#[derive(Debug, Default)]
pub struct ScriptedHttp {
    script: Mutex<Script>,
}

impl ScriptedHttp {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn script(&self) -> std::sync::MutexGuard<'_, Script> {
        self.script
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn push(&self, url: &str, reply: Reply) {
        self.script()
            .replies
            .entry(url.to_owned())
            .or_default()
            .push_back(reply);
    }

    /// Queue a response for `url` (as normalized by [`Url`]'s `Display`): `head` is the raw
    /// header block (`HTTP/1.1 200 OK\r\nContent-Type: image/png`), `body` the body bytes.
    pub fn respond(&self, url: &str, head: &str, body: &[u8]) {
        self.respond_after(url, Duration::ZERO, head, body);
    }

    /// Like [`ScriptedHttp::respond`], but the answer takes `delay` to arrive.
    pub fn respond_after(&self, url: &str, delay: Duration, head: &str, body: &[u8]) {
        self.push(
            url,
            Reply::Response {
                head: head.to_owned(),
                body: body.to_vec(),
                delay,
                broken: None,
            },
        );
    }

    /// Like [`ScriptedHttp::respond`], but the connection fails with `error` once `body` (the
    /// part of the body that arrives) has been read.
    pub fn respond_broken(&self, url: &str, head: &str, body: &[u8], error: NetError) {
        self.push(
            url,
            Reply::Response {
                head: head.to_owned(),
                body: body.to_vec(),
                delay: Duration::ZERO,
                broken: Some(error),
            },
        );
    }

    /// Queue a failure for `url`.
    pub fn fail(&self, url: &str, error: NetError) {
        self.push(url, Reply::Failure(error));
    }

    /// The URLs requested so far, in order.
    #[must_use]
    pub fn requests(&self) -> Vec<String> {
        self.script().requests.clone()
    }

    /// Record the request, take the next reply of its URL and wait for its delay.
    fn answer(&self, request: &HttpRequest<'_>) -> Result<ScriptedAnswer, NetError> {
        let key = request.url.to_string();
        let reply = {
            let mut script = self.script();
            script.requests.push(key.clone());
            script
                .replies
                .get_mut(&key)
                .and_then(VecDeque::pop_front)
                .unwrap_or(Reply::Failure(NetError::Os {
                    call: "connect",
                    code: 12029,
                }))
        };
        match reply {
            Reply::Failure(error) => Err(error),
            Reply::Response {
                head,
                body,
                delay,
                broken,
            } => {
                std::thread::sleep(delay);
                Ok((head, body, broken))
            }
        }
    }
}

/// What a scripted URL answered: the raw head, the body and the failure the connection ends with
/// once the body has been read.
type ScriptedAnswer = (String, Vec<u8>, Option<NetError>);

/// The reads of a connection that delivers `body` and then ends, or breaks with `broken`.
fn scripted_reads(
    body: &[u8],
    broken: Option<NetError>,
) -> impl FnMut(&mut [u8]) -> Result<usize, NetError> + '_ {
    let mut remaining = body;
    move |buffer| {
        let count = remaining.len().min(buffer.len());
        if count == 0 {
            // The body is used up: the end of the stream, or the break the script announced.
            return broken.clone().map_or(Ok(0), Err);
        }
        buffer[..count].copy_from_slice(&remaining[..count]);
        remaining = &remaining[count..];
        Ok(count)
    }
}

impl Http for ScriptedHttp {
    fn get(&self, request: &HttpRequest<'_>) -> Result<HttpResponse, NetError> {
        let started = Instant::now();
        let (head, body, broken) = self.answer(request)?;
        complete(&head, request, started, scripted_reads(&body, broken))
    }

    fn get_streaming(
        &self,
        request: &HttpRequest<'_>,
        sink: &mut BodySink<'_>,
    ) -> Result<ResponseHead, NetError> {
        let started = Instant::now();
        let (head, body, broken) = self.answer(request)?;
        complete_streaming(&head, request, started, scripted_reads(&body, broken), sink)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::{SetLastError, WIN32_ERROR};

    #[test]
    fn null_handle_reports_the_thread_last_error() {
        // SAFETY: writes only the calling thread's last-error value.
        unsafe { SetLastError(WIN32_ERROR(5)) };
        let result = check("WinHttpOpen", handle(std::ptr::null_mut()).map(|_| ()));
        assert_eq!(
            result,
            Err(NetError::Os {
                call: "WinHttpOpen",
                code: 5
            })
        );
    }

    #[test]
    fn an_opened_session_is_kept_by_the_exchange_and_closed_with_it() {
        // Opening a session sends nothing: the address is never contacted.
        let url = Url::parse("http://127.0.0.1:1/").unwrap();
        let mut exchange = Exchange {
            request: HttpRequest {
                url: &url,
                timeout: Duration::from_secs(1),
                max_body: 10,
                accept: Accept::Any,
            },
            http_request: None,
            connection: None,
            session: None,
            raw_head: String::new(),
        };
        assert!(raw_of(&exchange.session).is_null());
        open_session(&mut exchange).unwrap();
        assert!(!raw_of(&exchange.session).is_null());
        assert!(exchange.connection.is_none() && exchange.http_request.is_none());
    }

    #[test]
    fn only_loopback_hosts_bypass_the_system_proxy() {
        for host in ["127.0.0.1", "127.8.9.10", "::1", "localhost", "LocalHost"] {
            assert_eq!(access_type(host), WINHTTP_ACCESS_TYPE_NO_PROXY, "{host}");
        }
        for host in [
            "example.com",
            "192.168.1.5",
            "2001:db8::1",
            "localhost.example.com",
        ] {
            assert_eq!(
                access_type(host),
                WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                "{host}"
            );
        }
    }

    #[test]
    fn a_read_that_fails_ends_the_body_with_its_error() {
        let url = Url::parse("https://example.com/a.png").unwrap();
        let request = HttpRequest {
            url: &url,
            timeout: Duration::from_secs(30),
            max_body: 1000,
            accept: Accept::Any,
        };
        let failure = NetError::Os {
            call: "WinHttpReadData",
            code: 12030,
        };
        let mut reads = 0;
        let result = complete(
            "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\n",
            &request,
            Instant::now(),
            |buffer| {
                reads += 1;
                if reads == 1 {
                    buffer[..3].copy_from_slice(b"abc");
                    Ok(3)
                } else {
                    Err(failure.clone())
                }
            },
        );
        assert_eq!(result, Err(failure));
        assert_eq!(reads, 2, "the first chunk was taken, the second failed");
    }

    #[test]
    fn null_handle_with_timeout_code_is_a_timeout() {
        // SAFETY: writes only the calling thread's last-error value.
        unsafe { SetLastError(WIN32_ERROR(ERROR_WINHTTP_TIMEOUT)) };
        let result = check("WinHttpConnect", handle(std::ptr::null_mut()).map(|_| ()));
        assert_eq!(result, Err(NetError::Timeout));
    }
}
