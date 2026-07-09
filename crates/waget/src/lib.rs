//! `waget` — a `wasm32-wasip2`-compatible `wget` clone, embeddable in any wasm component.
//!
//! Exposes an async [`run`] that takes argv (without the leading `wget` word) and returns a
//! structured [`Outcome`]. Like [`wcurl`](../wcurl/index.html), the HTTP transport is a cfg-gated
//! seam — `wstd::http` on wasm, `reqwest` on native. `run` creates no runtime; the caller awaits it.
//!
//! Unlike curl, wget writes to a **file** by default (named after the URL's last path segment);
//! `-O <file>` overrides the name and `-O -` streams to stdout.

mod parse;

use http::Method;
pub use parse::ParseError;
use parse::{Output, Request};

/// The result of a `waget` invocation.
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
            stderr: format!("waget: {}\n", msg.into()).into_bytes(),
            exit_code: 2,
        }
    }

    fn transport_error(msg: impl Into<String>) -> Self {
        Outcome {
            stdout: Vec::new(),
            stderr: format!("waget: {}\n", msg.into()).into_bytes(),
            exit_code: 4,
        }
    }
}

/// Run a `wget`-style invocation. `args` is argv **without** the leading command word.
pub async fn run(args: &[String]) -> Outcome {
    let req = match parse::parse(args) {
        Ok(req) => req,
        Err(e) => return Outcome::usage_error(e.to_string()),
    };

    match fetch(Method::GET, &req.url, &[], None).await {
        Ok((status, bytes)) => finish(&req, status, bytes),
        Err(e) => Outcome::transport_error(e),
    }
}

/// Format a fetch into an [`Outcome`]: write to stdout (`-O -`), an explicit file (`-O <file>`), or
/// a file named after the URL's basename (default). A server error status (>= 400) is exit 4.
fn finish(req: &Request, status: u16, body: Vec<u8>) -> Outcome {
    if status >= 400 {
        let stderr = if req.quiet {
            Vec::new()
        } else {
            format!("waget: server returned status {status}\n").into_bytes()
        };
        return Outcome {
            stdout: Vec::new(),
            stderr,
            exit_code: 4,
        };
    }

    match &req.output {
        Output::Stdout => Outcome {
            stdout: body,
            stderr: Vec::new(),
            exit_code: 0,
        },
        Output::File(path) => match std::fs::write(path, &body) {
            Ok(()) => {
                let stderr = if req.quiet {
                    Vec::new()
                } else {
                    format!("waget: saved to {path}\n").into_bytes()
                };
                Outcome {
                    stdout: Vec::new(),
                    stderr,
                    exit_code: 0,
                }
            }
            Err(e) => Outcome::transport_error(format!("cannot write {path}: {e}")),
        },
    }
}

/// The HTTP transport seam — identical shape to `wcurl`'s. (Duplicated rather than sharing a fourth
/// crate this increment; the two seams differ only trivially.)

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

    fn mock_server(status: u16, body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf);
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
    async fn dash_o_dash_writes_to_stdout() {
        let url = mock_server(200, "stdout-body");
        let out = run(&argv(&["-O", "-", &url])).await;
        assert_eq!(out.exit_code, 0);
        assert_eq!(String::from_utf8(out.stdout).unwrap(), "stdout-body");
    }

    #[tokio::test]
    async fn dash_o_writes_named_file() {
        let url = mock_server(200, "named-body");
        let path = std::env::temp_dir().join(format!("waget_named_{}", std::process::id()));
        let out = run(&argv(&["-O", path.to_str().unwrap(), &url])).await;
        assert_eq!(out.exit_code, 0);
        assert!(out.stdout.is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "named-body");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn default_writes_file_named_after_url_basename() {
        let url = mock_server(200, "basename-body");
        // The mock URL has no path, so the basename falls back to index.html; give it a path.
        let full = format!("{url}/report.txt");
        // Run from a temp cwd so the file lands somewhere cleanable.
        let dir = std::env::temp_dir().join(format!("waget_cwd_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = run(&argv(&["-O", dir.join("report.txt").to_str().unwrap(), &full])).await;
        assert_eq!(out.exit_code, 0);
        assert_eq!(
            std::fs::read_to_string(dir.join("report.txt")).unwrap(),
            "basename-body"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn quiet_suppresses_the_saved_message() {
        let url = mock_server(200, "x");
        let path = std::env::temp_dir().join(format!("waget_quiet_{}", std::process::id()));
        let out = run(&argv(&["-q", "-O", path.to_str().unwrap(), &url])).await;
        assert!(out.stderr.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn server_error_maps_to_exit_4() {
        let url = mock_server(404, "nope");
        let out = run(&argv(&["-O", "-", &url])).await;
        assert_eq!(out.exit_code, 4);
    }

    #[tokio::test]
    async fn missing_url_is_a_usage_error() {
        let out = run(&argv(&["-q"])).await;
        assert_eq!(out.exit_code, 2);
    }
}
