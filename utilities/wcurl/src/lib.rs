//! `wcurl` — a `wasm32-wasip2`-compatible `curl` clone, embeddable in any wasm component.
//!
//! Exposes an async [`run`] that takes argv (without the leading `curl` word) and returns a
//! structured [`Outcome`] (stdout bytes, stderr bytes, exit code). The HTTP transport — the
//! cfg-gated `wstd`/`reqwest` seam plus redirect following and timeouts — lives in [`whttp`]; this
//! crate parses `curl` flags and formats the response.
//!
//! `run` is `async` and does NOT create its own runtime: the caller awaits it under whatever async
//! executor is running (clank awaits it one level under the Golem SDK's `wstd::block_on`, where the
//! wstd reactor is live; the standalone `main` wraps it in its own `block_on`).

mod parse;

use std::fmt::Write as _;

pub use parse::ParseError;
use parse::Request;

/// The result of a `wcurl` invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    /// Bytes written to standard output (the response body and any `-w` output).
    pub stdout: Vec<u8>,
    /// Bytes written to standard error (status/transport notes and any `-v` trace).
    pub stderr: Vec<u8>,
    /// The process exit code (0 ok; 2 usage error; 4 transport/HTTP error; 22 `-f` fail).
    pub exit_code: u8,
}

impl Outcome {
    fn usage_error(msg: impl Into<String>) -> Self {
        Outcome {
            stdout: Vec::new(),
            stderr: format!("wcurl: {}\n", msg.into()).into_bytes(),
            exit_code: 2,
        }
    }

    fn transport_error(msg: impl Into<String>) -> Self {
        Outcome {
            stdout: Vec::new(),
            stderr: format!("wcurl: {}\n", msg.into()).into_bytes(),
            exit_code: 4,
        }
    }
}

/// Run a `curl`-style invocation. `args` is argv **without** the leading command word.
pub async fn run(args: &[String]) -> Outcome {
    let req = match parse::parse(args) {
        Ok(req) => req,
        Err(e) => return Outcome::usage_error(e.to_string()),
    };

    // `-G`: fold any `-d` data into the query string of a GET and send no body.
    let (url, body) = if req.get {
        match &req.data {
            Some(d) => {
                let sep = if req.url.contains('?') { '&' } else { '?' };
                (format!("{}{}{}", req.url, sep, String::from_utf8_lossy(d)), None)
            }
            None => (req.url.clone(), None),
        }
    } else {
        (req.url.clone(), req.data.clone())
    };

    let mut hreq = whttp::Request::new(req.method.clone(), url);
    hreq.headers = req.headers.clone();
    hreq.body = body;
    hreq.follow_redirects = req.location;
    // Pin curl's redirect cap explicitly rather than inheriting `whttp::Request::new`'s default —
    // otherwise `curl -L`'s bound is an invisible dependency on a constant in another crate, and it
    // silently diverges from waget (which sets 20). 50 matches curl's own default (audit P3-5).
    hreq.max_redirects = 50;
    hreq.connect_timeout = req.connect_timeout.map(std::time::Duration::from_secs_f64);
    hreq.timeout = req.max_time.map(std::time::Duration::from_secs_f64);

    match whttp::fetch(&hreq).await {
        Ok(resp) => finish(&req, resp),
        Err(e) => Outcome::transport_error(e.to_string()),
    }
}

/// Format a fetch into an [`Outcome`], honoring the output-shaping flags. Without `-f`, a client/
/// server error status (>= 400) still writes the body but yields exit 4 (clank's "remote call
/// failed"); `-f` suppresses the body and yields curl's exit 22.
// resp is a private helper's owned response; taking it by value keeps the single call site simple.
#[allow(clippy::needless_pass_by_value)]
fn finish(req: &Request, resp: whttp::Response) -> Outcome {
    let status = resp.status;
    let mut stdout: Vec<u8> = Vec::new();
    let mut stderr: Vec<u8> = Vec::new();

    if req.verbose {
        stderr.extend_from_slice(verbose_trace(req, &resp).as_bytes());
    }

    // -f/--fail: no body on an HTTP error; exit 22.
    if req.fail && status >= 400 {
        if !req.silent {
            stderr.extend_from_slice(format!("wcurl: server returned status {status}\n").as_bytes());
        }
        if let Some(fmt) = &req.write_out {
            stdout.extend_from_slice(write_out(fmt, &resp).as_bytes());
        }
        return Outcome { stdout, stderr, exit_code: 22 };
    }

    // The payload: optional header block (-i, or -I which is headers-only), then the body unless
    // this was a HEAD.
    let mut payload: Vec<u8> = Vec::new();
    if req.include || req.head {
        payload.extend_from_slice(header_block(&resp).as_bytes());
    }
    if !req.head {
        payload.extend_from_slice(&resp.body);
    }

    match &req.output {
        Some(path) => {
            if let Err(e) = std::fs::write(path, &payload) {
                return Outcome::transport_error(format!("cannot write {path}: {e}"));
            }
        }
        None => stdout.extend_from_slice(&payload),
    }

    // -w/--write-out is appended to stdout after the transfer.
    if let Some(fmt) = &req.write_out {
        stdout.extend_from_slice(write_out(fmt, &resp).as_bytes());
    }

    // The status is already visible under -i/-I, so only surface the stderr note otherwise.
    if status >= 400 && !req.silent && !req.include && !req.head {
        stderr.extend_from_slice(format!("wcurl: server returned status {status}\n").as_bytes());
    }
    let exit_code = if status >= 400 { 4 } else { 0 };
    Outcome { stdout, stderr, exit_code }
}

/// The status line + response headers, as curl prints them under `-i`/`-I` (CRLF, blank line last).
fn header_block(resp: &whttp::Response) -> String {
    let mut s = format!("HTTP/1.1 {}\r\n", resp.status);
    for (k, v) in &resp.headers {
        let _ = write!(s, "{k}: {v}\r\n");
    }
    s.push_str("\r\n");
    s
}

/// A `-v` trace: request line + request headers (`>`), then response status + headers (`<`).
fn verbose_trace(req: &Request, resp: &whttp::Response) -> String {
    let mut s = format!("> {} {}\n", req.method, req.url);
    for (k, v) in &req.headers {
        let _ = write!(s, "> {k}: {v}\n");
    }
    let _ = write!(s, "< HTTP/1.1 {}\n", resp.status);
    for (k, v) in &resp.headers {
        let _ = write!(s, "< {k}: {v}\n");
    }
    s
}

/// Expand a `-w`/`--write-out` format string: `%{var}` variables, `%%`, and `\n`/`\t`/`\r` escapes.
/// Unknown variables expand to empty (curl warns; we stay quiet).
fn write_out(fmt: &str, resp: &whttp::Response) -> String {
    let mut out = String::new();
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some(other) => out.push(other),
                None => out.push('\\'),
            },
            '%' if chars.peek() == Some(&'%') => {
                chars.next();
                out.push('%');
            }
            '%' if chars.peek() == Some(&'{') => {
                chars.next();
                let mut var = String::new();
                for c in chars.by_ref() {
                    if c == '}' {
                        break;
                    }
                    var.push(c);
                }
                out.push_str(&write_out_var(&var, resp));
            }
            other => out.push(other),
        }
    }
    out
}

fn write_out_var(var: &str, resp: &whttp::Response) -> String {
    match var {
        "http_code" | "response_code" => resp.status.to_string(),
        "size_download" => resp.body.len().to_string(),
        "url_effective" => resp.final_url.clone(),
        "content_type" => resp.header("content-type").unwrap_or("").to_string(),
        _ => String::new(),
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// Spin a one-shot localhost HTTP/1.1 server on an ephemeral port that replies with `status` and
    /// `body` to the first request. Returns the bound `http://127.0.0.1:<port>` base URL. Hermetic:
    /// no real-internet dependency. The server thread exits after serving one request.
    fn mock_server(status: u16, body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf); // drain the request head (ignored)
                let response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        format!("http://{addr}")
    }

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(std::string::ToString::to_string).collect()
    }

    #[tokio::test]
    async fn get_writes_body_to_stdout() {
        let url = mock_server(200, "hello-body");
        let out = run(&argv(&[&url])).await;
        assert_eq!(out.exit_code, 0);
        assert_eq!(String::from_utf8(out.stdout).unwrap(), "hello-body");
    }

    #[tokio::test]
    async fn output_flag_writes_file_and_leaves_stdout_empty() {
        let url = mock_server(200, "file-body");
        let path = std::env::temp_dir().join(format!("wcurl_out_{}", std::process::id()));
        let out = run(&argv(&["-o", path.to_str().unwrap(), &url])).await;
        assert_eq!(out.exit_code, 0);
        assert!(out.stdout.is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "file-body");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn server_error_status_maps_to_exit_4() {
        let url = mock_server(500, "boom");
        let out = run(&argv(&[&url])).await;
        assert_eq!(out.exit_code, 4);
        // The body is still written to stdout (curl behavior).
        assert_eq!(String::from_utf8(out.stdout).unwrap(), "boom");
    }

    #[tokio::test]
    async fn silent_suppresses_the_status_error_message() {
        let url = mock_server(500, "boom");
        let out = run(&argv(&["-s", &url])).await;
        assert_eq!(out.exit_code, 4);
        assert!(out.stderr.is_empty(), "-s should suppress the status message");
    }

    #[tokio::test]
    async fn missing_url_is_a_usage_error() {
        let out = run(&argv(&["-s"])).await;
        assert_eq!(out.exit_code, 2);
    }

    #[tokio::test]
    async fn unroutable_host_is_a_transport_error() {
        // Port 1 on localhost refuses — a connection failure, not an HTTP status.
        let out = run(&argv(&["http://127.0.0.1:1/"])).await;
        assert_eq!(out.exit_code, 4);
        assert!(out.stdout.is_empty());
    }

    /// A one-shot server that also sends an extra response header (for `-i`/`-w` assertions).
    fn mock_server_h(status: u16, header: (&'static str, &'static str), body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf);
                let response = format!(
                    "HTTP/1.1 {status} X\r\n{}: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    header.0,
                    header.1,
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        format!("http://{addr}")
    }

    /// A server that 302-redirects the first request to `/final`, then serves `body` (for `-L`).
    fn mock_redirect(body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let loc = format!("{base}/final");
        std::thread::spawn(move || {
            for i in 0..2 {
                if let Ok((mut stream, _)) = listener.accept() {
                    let mut buf = [0u8; 2048];
                    let _ = stream.read(&mut buf);
                    let response = if i == 0 {
                        format!("HTTP/1.1 302 X\r\nLocation: {loc}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    } else {
                        format!("HTTP/1.1 200 X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
                    };
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                }
            }
        });
        base
    }

    #[tokio::test]
    async fn include_prepends_response_headers() {
        let url = mock_server_h(200, ("X-Custom", "yes"), "the-body");
        let out = run(&argv(&["-i", &url])).await;
        let text = String::from_utf8(out.stdout).unwrap();
        assert!(text.starts_with("HTTP/1.1 200"), "status line first:\n{text}");
        assert!(text.to_lowercase().contains("x-custom: yes"), "header shown:\n{text}");
        assert!(text.contains("the-body"), "body follows:\n{text}");
    }

    #[tokio::test]
    async fn head_prints_headers_without_body() {
        let url = mock_server_h(200, ("X-Custom", "yes"), "should-not-appear");
        let out = run(&argv(&["-I", &url])).await;
        let text = String::from_utf8(out.stdout).unwrap();
        assert!(text.to_lowercase().contains("x-custom: yes"));
        assert!(!text.contains("should-not-appear"), "HEAD has no body:\n{text}");
    }

    #[tokio::test]
    async fn fail_flag_suppresses_body_and_exits_22() {
        let url = mock_server(404, "error page");
        let out = run(&argv(&["-f", &url])).await;
        assert_eq!(out.exit_code, 22);
        assert!(out.stdout.is_empty(), "-f writes no body");
    }

    #[tokio::test]
    async fn write_out_expands_http_code() {
        let url = mock_server(200, "body");
        let out = run(&argv(&["-s", "-o", "/dev/null", "-w", "%{http_code}\\n", &url])).await;
        assert_eq!(String::from_utf8(out.stdout).unwrap(), "200\n");
    }

    #[tokio::test]
    async fn location_follows_the_redirect() {
        let url = mock_redirect("arrived");
        // Without -L: the 302 body (empty) and exit 0.
        let out = run(&argv(&["-s", &mock_redirect("x")])).await;
        assert_eq!(out.exit_code, 0);
        // With -L: the final body.
        let out = run(&argv(&["-sL", &url])).await;
        assert_eq!(String::from_utf8(out.stdout).unwrap(), "arrived");
    }
}
