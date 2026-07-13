//! `wcurl` — a `wasm32-wasip2`-compatible `curl` clone, embeddable in any wasm component.
//!
//! Exposes an async [`run`] that takes argv (without the leading `curl` word) and returns a
//! structured [`Outcome`] (stdout bytes, stderr bytes, exit code). The HTTP transport is a
//! cfg-gated seam — `wstd::http` on wasm (the only client that works inside a Golem/wasip2
//! component), `reqwest` on native — matching clank's "small trait, two implementations" model.
//!
//! `run` is `async` and does NOT create its own runtime: the caller awaits it under whatever async
//! executor is running (clank awaits it one level under the Golem SDK's `wstd::block_on`, where the
//! wstd reactor is live; the standalone `main` wraps it in its own `block_on`).

mod parse;

use http::Method;
pub use parse::ParseError;
use parse::Request;

/// The result of a `wcurl` invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
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

    let body = req.data.clone().map(String::into_bytes);
    let headers: Vec<(String, String)> = req.headers.clone();
    let method = req.method.clone();

    match fetch(method, &req.url, &headers, body).await {
        Ok((status, bytes)) => finish(&req, status, bytes),
        Err(e) => Outcome::transport_error(e),
    }
}

/// Format a successful fetch into an [`Outcome`], honoring `-o`, `-s`, and mapping HTTP status to an
/// exit code. curl writes the body to stdout (or a file) regardless of status; a client/server
/// error status (>= 400) yields exit 4 ("remote call failed", per clank's exit-code table), while
/// 2xx/3xx yield exit 0.
fn finish(req: &Request, status: u16, body: Vec<u8>) -> Outcome {
    let mut stderr = Vec::new();
    let mut stdout = Vec::new();

    match &req.output {
        Some(path) => {
            if let Err(e) = std::fs::write(path, &body) {
                return Outcome::transport_error(format!("cannot write {path}: {e}"));
            }
        }
        None => stdout = body,
    }

    if status >= 400 && !req.silent {
        stderr = format!("wcurl: server returned status {status}\n").into_bytes();
    }
    let exit_code = if status >= 400 { 4 } else { 0 };
    Outcome {
        stdout,
        stderr,
        exit_code,
    }
}

/// The HTTP transport seam. Returns `(status, body_bytes)` or a transport-error message.

#[cfg(target_arch = "wasm32")]
async fn fetch(
    method: Method,
    url: &str,
    headers: &[(String, String)],
    body: Option<Vec<u8>>,
) -> Result<(u16, Vec<u8>), String> {
    use wstd::http::{Body, Client, Request as WstdRequest};

    let mut builder = WstdRequest::builder().method(method).uri(url);
    for (k, v) in headers {
        builder = builder.header(k, v);
    }
    let wstd_body = match body {
        Some(bytes) => Body::from(bytes),
        None => Body::empty(),
    };
    let request = builder
        .body(wstd_body)
        .map_err(|e| format!("bad request: {e}"))?;

    let mut response = Client::new()
        .send(request)
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let status = response.status().as_u16();
    let bytes = response
        .body_mut()
        .contents()
        .await
        .map_err(|e| format!("reading response failed: {e}"))?
        .to_vec();
    Ok((status, bytes))
}

#[cfg(not(target_arch = "wasm32"))]
async fn fetch(
    method: Method,
    url: &str,
    headers: &[(String, String)],
    body: Option<Vec<u8>>,
) -> Result<(u16, Vec<u8>), String> {
    let client = reqwest::Client::builder()
        .build()
        .map_err(|e| format!("client init failed: {e}"))?;
    let mut builder = client.request(method, url);
    for (k, v) in headers {
        builder = builder.header(k.as_str(), v.as_str());
    }
    if let Some(bytes) = body {
        builder = builder.body(bytes);
    }
    let response = builder
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let status = response.status().as_u16();
    let bytes = response
        .bytes()
        .await
        .map_err(|e| format!("reading response failed: {e}"))?
        .to_vec();
    Ok((status, bytes))
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
        parts.iter().map(|s| s.to_string()).collect()
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
}
