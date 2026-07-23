//! `whttp` — the shared HTTP transport behind `wcurl` and `waget`.
//!
//! One cfg-gated client seam (`wstd::http` on wasm, `reqwest` on native) plus **target-agnostic**
//! redirect following and timeouts, so the two command clients behave identically on native and on
//! the Golem/wasip2 agent. This is the "fourth crate" the duplicated seams in `wcurl`/`waget`
//! anticipated.
//!
//! Redirect following lives here, not in the transport: `wstd` does not follow redirects at all and
//! `reqwest` follows by default, so leaving it to the transport made `curl`/`wget` behave
//! differently on the two targets. The native path disables reqwest's auto-follow so this loop is
//! the single source of truth. `Location` resolution is hand-rolled (see [`resolve_url`]) to keep
//! the `url` crate's `idna`→`icu` Unicode tables out of the wasm agent.
//!
//! [`fetch`] is `async` and creates no runtime — the caller awaits it under whatever executor is
//! live (clank awaits one level under the Golem SDK's `wstd::block_on`).

use http::Method;
use std::time::Duration;

/// A request to perform, including redirect and timeout policy.
#[derive(Clone, Debug)]
pub struct Request {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    /// Follow 3xx `Location` responses (curl `-L`; wget follows by default).
    pub follow_redirects: bool,
    /// Cap on redirects when following, to bound a redirect loop.
    pub max_redirects: u32,
    /// Time budget for establishing the connection.
    pub connect_timeout: Option<Duration>,
    /// Overall time budget for the request (maps to reqwest's total timeout; on wasm, the
    /// first-byte timeout — the closest WASI-HTTP primitive).
    pub timeout: Option<Duration>,
}

impl Request {
    /// A plain `GET` with no redirect following and no timeouts — the base every client tweaks.
    pub fn new(method: Method, url: impl Into<String>) -> Self {
        Request {
            method,
            url: url.into(),
            headers: Vec::new(),
            body: None,
            follow_redirects: false,
            max_redirects: 50,
            connect_timeout: None,
            timeout: None,
        }
    }
}

/// A completed HTTP response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// The URL the response actually came from (differs from the request URL after redirects) —
    /// used for `-w`, `--content-disposition`, and wget's default filename.
    pub final_url: String,
}

impl Response {
    /// Case-insensitive header lookup (the first match wins).
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// A transport or policy failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// Connection refused, DNS failure, TLS error, timeout, unreadable body, …
    Transport(String),
    /// `follow_redirects` was set and the chain exceeded `max_redirects`.
    TooManyRedirects(u32),
    /// A `Location` header could not be resolved against the current URL.
    BadRedirect(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Transport(m) => write!(f, "{m}"),
            Error::TooManyRedirects(n) => write!(f, "too many redirects (exceeded {n})"),
            Error::BadRedirect(loc) => write!(f, "could not resolve redirect to {loc}"),
        }
    }
}

impl std::error::Error for Error {}

/// Perform `req`, following redirects per its policy. Returns the final [`Response`].
pub async fn fetch(req: &Request) -> Result<Response, Error> {
    let mut method = req.method.clone();
    let mut url = req.url.clone();
    let mut body = req.body.clone();
    let mut redirects = 0u32;

    loop {
        let resp = fetch_once(
            &method,
            &url,
            &req.headers,
            body.clone(),
            req.connect_timeout,
            req.timeout,
        )
        .await?;

        if req.follow_redirects && is_redirect(resp.status) {
            if let Some(location) = resp.header("location") {
                if redirects >= req.max_redirects {
                    return Err(Error::TooManyRedirects(req.max_redirects));
                }
                redirects += 1;
                let next = resolve_url(&url, location)
                    .ok_or_else(|| Error::BadRedirect(location.to_string()))?;
                (method, body) = redirect_method(&method, resp.status, body);
                url = next;
                continue;
            }
        }

        return Ok(Response {
            final_url: url,
            ..resp
        });
    }
}

/// The redirect status codes a client following redirects acts on.
fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

/// Whether the response carries no body by spec: a HEAD request, or a `204 No Content` / `304 Not
/// Modified` status. Reading a body that never comes half-consumes the connection.
fn is_bodyless(method: &Method, status: u16) -> bool {
    *method == Method::HEAD || matches!(status, 204 | 304)
}

/// The method and body for the next hop, following curl/browser convention: a `303` (and a `POST`
/// under `301`/`302`) becomes a bodyless `GET`; `307`/`308` (and non-`POST` `301`/`302`) preserve
/// the method and body.
fn redirect_method(method: &Method, status: u16, body: Option<Vec<u8>>) -> (Method, Option<Vec<u8>>) {
    match status {
        303 => (Method::GET, None),
        301 | 302 if *method == Method::POST => (Method::GET, None),
        _ => (method.clone(), body),
    }
}

/// Resolve a `Location` value (absolute, network-path, absolute-path, or relative) against the
/// current absolute URL — RFC 3986 §5 for the cases redirects actually use. Hand-rolled rather than
/// pulling the `url` crate, whose `idna`→`icu` dependency added ~180 MB of Unicode tables to the
/// wasm agent for what is a handful of string operations here.
fn resolve_url(base: &str, location: &str) -> Option<String> {
    let loc = location.trim();
    if loc.is_empty() {
        return Some(base.to_string());
    }
    // Absolute URL — use as-is.
    if has_scheme(loc) {
        return Some(loc.to_string());
    }

    let (scheme, rest) = base.split_once("://")?;
    let (authority, base_path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let base_path = base_path.split(['?', '#']).next().unwrap_or("/");

    // Network-path reference (`//host/path`): inherit only the scheme.
    if let Some(after) = loc.strip_prefix("//") {
        return Some(format!("{scheme}://{after}"));
    }
    // Absolute path.
    if loc.starts_with('/') {
        return Some(format!("{scheme}://{authority}{}", remove_dot_segments(loc)));
    }
    // Query- or fragment-only reference: keep the base path.
    if loc.starts_with('?') || loc.starts_with('#') {
        return Some(format!("{scheme}://{authority}{base_path}{loc}"));
    }
    // Relative path: merge against the base's directory, then normalize dot-segments.
    let dir = match base_path.rfind('/') {
        Some(i) => &base_path[..=i],
        None => "/",
    };
    let merged = remove_dot_segments(&format!("{dir}{loc}"));
    Some(format!("{scheme}://{authority}{merged}"))
}

/// Whether `s` begins with a URI scheme (`scheme://…`): an ALPHA followed by ALPHA/DIGIT/`+`/`-`/`.`
/// up to `://`.
fn has_scheme(s: &str) -> bool {
    match s.find("://") {
        Some(i) if i > 0 => {
            let scheme = s.as_bytes();
            scheme[0].is_ascii_alphabetic()
                && s[..i]
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'-' | b'.'))
        }
        _ => false,
    }
}

/// RFC 3986 §5.2.4 dot-segment removal on an absolute path (`.`/`..`), preserving the leading slash
/// and a trailing slash.
fn remove_dot_segments(path: &str) -> String {
    let trailing_slash = path.ends_with('/');
    let mut out: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    let mut result = String::from("/");
    result.push_str(&out.join("/"));
    if trailing_slash && !result.ends_with('/') {
        result.push('/');
    }
    result
}

// ---------------------------------------------------------------------------------------------
// The cfg-gated transport: a SINGLE request/response, no redirect handling (the loop above owns
// that). Returns a `Response` whose `final_url` the caller overwrites after the loop settles.
// ---------------------------------------------------------------------------------------------

#[cfg(target_arch = "wasm32")]
async fn fetch_once(
    method: &Method,
    url: &str,
    headers: &[(String, String)],
    body: Option<Vec<u8>>,
    connect_timeout: Option<Duration>,
    timeout: Option<Duration>,
) -> Result<Response, Error> {
    use wstd::http::{Body, Client, Request as WstdRequest};

    let mut client = Client::new();
    if let Some(d) = connect_timeout {
        client.set_connect_timeout(d);
    }
    if let Some(d) = timeout {
        // The closest WASI-HTTP primitive to a total deadline is the first-byte timeout.
        client.set_first_byte_timeout(d);
    }

    let mut builder = WstdRequest::builder().method(method.clone()).uri(url);
    for (k, v) in headers {
        builder = builder.header(k, v);
    }
    let wstd_body = match body {
        Some(bytes) => Body::from(bytes),
        None => Body::empty(),
    };
    let request = builder
        .body(wstd_body)
        .map_err(|e| Error::Transport(format!("bad request: {e}")))?;

    let mut response = client
        .send(request)
        .await
        .map_err(|e| Error::Transport(format!("request failed: {e}")))?;
    let status = response.status().as_u16();
    let headers = collect_headers(response.headers());
    // A HEAD/204/304 response has NO body. Reading the stream anyway waits on `Content-Length`
    // bytes that never arrive, leaving the connection half-consumed — which on WASI-HTTP wedges
    // the NEXT outbound request. Skip the read for bodyless responses.
    let bytes = if is_bodyless(method, status) {
        Vec::new()
    } else {
        response
            .body_mut()
            .contents()
            .await
            .map_err(|e| Error::Transport(format!("reading response failed: {e}")))?
            .to_vec()
    };
    Ok(Response {
        status,
        headers,
        body: bytes,
        final_url: url.to_string(),
    })
}

#[cfg(not(target_arch = "wasm32"))]
async fn fetch_once(
    method: &Method,
    url: &str,
    headers: &[(String, String)],
    body: Option<Vec<u8>>,
    connect_timeout: Option<Duration>,
    timeout: Option<Duration>,
) -> Result<Response, Error> {
    // `redirect(none)` is load-bearing: the shared loop above is the single source of redirect
    // behavior across both targets, so reqwest's default auto-follow must be off.
    let mut cb = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none());
    // Apply a DEFAULT bound when the caller passed none. An unbounded outbound request is a real
    // hazard for a long-lived/durable host: a hung peer holds the connection forever, and because a
    // durable agent serializes invocations, it wedges every request queued behind it (audit P2-4).
    // Callers that want a different bound (wcurl `-m`, waget `-T`) still override these.
    let connect_timeout = connect_timeout.or(Some(Duration::from_secs(30)));
    let timeout = timeout.or(Some(Duration::from_mins(5)));
    if let Some(d) = connect_timeout {
        cb = cb.connect_timeout(d);
    }
    if let Some(d) = timeout {
        cb = cb.timeout(d);
    }
    let client = cb
        .build()
        .map_err(|e| Error::Transport(format!("client init failed: {e}")))?;

    let mut builder = client.request(method.clone(), url);
    for (k, v) in headers {
        builder = builder.header(k.as_str(), v.as_str());
    }
    if let Some(bytes) = body {
        builder = builder.body(bytes);
    }
    let response = builder
        .send()
        .await
        .map_err(|e| Error::Transport(format!("request failed: {e}")))?;
    let status = response.status().as_u16();
    let headers = collect_headers(response.headers());
    // A HEAD/204/304 response has no body — skip the read (matches the wasm path; see the note
    // there on why reading a phantom body is harmful).
    let bytes = if is_bodyless(method, status) {
        Vec::new()
    } else {
        response
            .bytes()
            .await
            .map_err(|e| Error::Transport(format!("reading response failed: {e}")))?
            .to_vec()
    };
    Ok(Response {
        status,
        headers,
        body: bytes,
        final_url: url.to_string(),
    })
}

/// Flatten an `http::HeaderMap` into `(name, value)` pairs, dropping any header whose value is not
/// valid UTF-8 (non-ASCII header values are vanishingly rare and unusable as text anyway).
fn collect_headers(map: &http::HeaderMap) -> Vec<(String, String)> {
    // `HeaderMap::iter()` yields headers in an UNSPECIFIED (hash) order that can differ between two
    // executions of the same request. On the durable agent that is a replay hazard: the first run
    // records this Vec (e.g. as a `curl -I` eval result) in the oplog, and a later resume re-executes
    // the request — a different iteration order produces a non-matching result and Golem refuses to
    // resume (`Unexpected oplog entry`, INTERNAL_AGENT_RESUME_FAILED). Sort to a stable order so the
    // recorded and replayed outputs are byte-identical. (curl/wget don't promise the wire order
    // anyway, and a stable order also makes `-I`/`-i` output reproducible for tests and demos.)
    let mut headers: Vec<(String, String)> = map
        .iter()
        .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.as_str().to_string(), v.to_string())))
        .collect();
    headers.sort();
    headers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_status_detection() {
        for s in [301, 302, 303, 307, 308] {
            assert!(is_redirect(s), "{s} should be a redirect");
        }
        for s in [200, 201, 300, 304, 400, 500] {
            assert!(!is_redirect(s), "{s} should not be a redirect");
        }
    }

    #[test]
    fn post_downgrades_to_get_on_301_302_303() {
        for s in [301, 302, 303] {
            let (m, b) = redirect_method(&Method::POST, s, Some(b"x".to_vec()));
            assert_eq!(m, Method::GET, "POST under {s} → GET");
            assert_eq!(b, None, "body dropped under {s}");
        }
    }

    #[test]
    fn method_and_body_preserved_on_307_308() {
        for s in [307, 308] {
            let (m, b) = redirect_method(&Method::POST, s, Some(b"x".to_vec()));
            assert_eq!(m, Method::POST, "POST preserved under {s}");
            assert_eq!(b, Some(b"x".to_vec()));
        }
    }

    #[test]
    fn non_post_methods_kept_on_301_302() {
        let (m, b) = redirect_method(&Method::PUT, 302, Some(b"x".to_vec()));
        assert_eq!(m, Method::PUT);
        assert_eq!(b, Some(b"x".to_vec()));
    }

    #[test]
    fn resolves_absolute_and_relative_locations() {
        assert_eq!(
            resolve_url("http://a.test/one/two", "https://b.test/x").as_deref(),
            Some("https://b.test/x")
        );
        assert_eq!(
            resolve_url("http://a.test/one/two", "/x").as_deref(),
            Some("http://a.test/x")
        );
        assert_eq!(
            resolve_url("http://a.test/one/two", "three").as_deref(),
            Some("http://a.test/one/three")
        );
        // Host-only base gets a root path.
        assert_eq!(
            resolve_url("http://a.test", "/x").as_deref(),
            Some("http://a.test/x")
        );
        // Network-path reference inherits the scheme.
        assert_eq!(
            resolve_url("https://a.test/p", "//b.test/q").as_deref(),
            Some("https://b.test/q")
        );
        // Query-only reference keeps the base path.
        assert_eq!(
            resolve_url("http://a.test/p/page", "?x=1").as_deref(),
            Some("http://a.test/p/page?x=1")
        );
        // Dot-segments are normalized.
        assert_eq!(
            resolve_url("http://a.test/one/two/three", "../x").as_deref(),
            Some("http://a.test/one/x")
        );
        assert_eq!(
            resolve_url("http://a.test/a/b/", "./c/../d").as_deref(),
            Some("http://a.test/a/b/d")
        );
    }

    #[test]
    fn scheme_detection() {
        assert!(has_scheme("https://x"));
        assert!(has_scheme("http://x"));
        assert!(!has_scheme("//x"));
        assert!(!has_scheme("/path"));
        assert!(!has_scheme("relative"));
    }

    #[test]
    fn header_lookup_is_case_insensitive() {
        let r = Response {
            status: 200,
            headers: vec![("Content-Type".into(), "text/html".into())],
            body: Vec::new(),
            final_url: "http://a.test".into(),
        };
        assert_eq!(r.header("content-type"), Some("text/html"));
        assert_eq!(r.header("CONTENT-TYPE"), Some("text/html"));
        assert_eq!(r.header("x-missing"), None);
    }
}

/// End-to-end transport tests against a hermetic localhost server (native only). The mock serves a
/// SCRIPTED sequence of responses to successive requests, so a redirect chain can be exercised
/// without the internet.
#[cfg(all(test, not(target_arch = "wasm32")))]
mod transport_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// One scripted response: status, extra headers, body.
    type Reply = (u16, Vec<(String, String)>, String);

    /// Bind an ephemeral localhost server that answers `replies` to successive requests in order,
    /// then closes. Returns the `http://127.0.0.1:<port>` base. `${BASE}` in a header value is
    /// replaced with that base (so a `Location` can point back at the mock).
    fn scripted_server(replies: Vec<Reply>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let base_for_thread = base.clone();
        std::thread::spawn(move || {
            for (status, headers, body) in replies {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf); // drain request head
                let mut head = format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\n", body.len());
                for (k, v) in headers {
                    let v = v.replace("${BASE}", &base_for_thread);
                    head.push_str(&format!("{k}: {v}\r\n"));
                }
                head.push_str("Connection: close\r\n\r\n");
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(body.as_bytes());
                let _ = stream.flush();
            }
        });
        base
    }

    fn reply(status: u16, headers: &[(&str, &str)], body: &str) -> Reply {
        (
            status,
            headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            body.to_string(),
        )
    }

    #[tokio::test]
    async fn does_not_follow_redirects_by_default() {
        let base = scripted_server(vec![reply(302, &[("Location", "${BASE}/next")], "go")]);
        let resp = fetch(&Request::new(Method::GET, base)).await.unwrap();
        assert_eq!(resp.status, 302);
        assert_eq!(resp.header("location").map(|l| l.ends_with("/next")), Some(true));
    }

    #[tokio::test]
    async fn follows_a_redirect_chain_to_the_final_body() {
        let base = scripted_server(vec![
            reply(301, &[("Location", "${BASE}/a")], "one"),
            reply(302, &[("Location", "${BASE}/b")], "two"),
            reply(200, &[], "arrived"),
        ]);
        let mut req = Request::new(Method::GET, base.clone());
        req.follow_redirects = true;
        let resp = fetch(&req).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(String::from_utf8(resp.body).unwrap(), "arrived");
        assert!(resp.final_url.ends_with("/b"), "final_url tracks the last hop: {}", resp.final_url);
    }

    #[tokio::test]
    async fn too_many_redirects_is_an_error() {
        // A server that always redirects to itself.
        let base = scripted_server(vec![reply(302, &[("Location", "${BASE}/loop")], ""); 5]);
        let mut req = Request::new(Method::GET, base);
        req.follow_redirects = true;
        req.max_redirects = 2;
        assert_eq!(fetch(&req).await, Err(Error::TooManyRedirects(2)));
    }

    #[tokio::test]
    async fn response_headers_are_captured() {
        let base = scripted_server(vec![reply(200, &[("X-Custom", "yes")], "body")]);
        let resp = fetch(&Request::new(Method::GET, base)).await.unwrap();
        assert_eq!(resp.header("x-custom"), Some("yes"));
    }

    #[tokio::test]
    async fn head_returns_headers_but_no_body() {
        // The mock still puts bytes on the wire; a HEAD must not read them (returns empty body),
        // which is both correct and avoids half-consuming the connection.
        let base = scripted_server(vec![reply(200, &[("X-Custom", "yes")], "should-not-be-read")]);
        let resp = fetch(&Request::new(Method::HEAD, base)).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.header("x-custom"), Some("yes"));
        assert!(resp.body.is_empty(), "HEAD body must be empty, got {:?}", resp.body);
    }
}
