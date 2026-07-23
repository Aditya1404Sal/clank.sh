//! `waget` — a `wasm32-wasip2`-compatible `wget` clone, embeddable in any wasm component.
//!
//! Exposes an async [`run`] that takes argv (without the leading `wget` word) and returns a
//! structured [`Outcome`]. The HTTP transport — the cfg-gated `wstd`/`reqwest` seam plus redirect
//! following and timeouts — lives in [`whttp`]; this crate parses `wget` flags and writes the file.
//! `run` creates no runtime; the caller awaits it.
//!
//! Unlike curl, wget writes to a **file** by default (named after the URL's last path segment);
//! `-O <file>` overrides the name and `-O -` streams to stdout.

mod parse;

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

    let mut hreq = whttp::Request::new(req.method.clone(), req.url.clone());
    hreq.headers = req.headers.clone();
    hreq.body = req.data.clone();
    hreq.follow_redirects = req.follow_redirects;
    hreq.max_redirects = req.max_redirect;
    if let Some(secs) = req.timeout {
        // wget's `-T` covers connect and read alike.
        let d = std::time::Duration::from_secs_f64(secs);
        hreq.connect_timeout = Some(d);
        hreq.timeout = Some(d);
    }

    // `-t`/`--tries`: re-attempt on a transport failure (default 1 = no retry).
    let mut attempt = 0;
    let resp = loop {
        match whttp::fetch(&hreq).await {
            Ok(resp) => break resp,
            Err(e) => {
                attempt += 1;
                if attempt >= req.tries {
                    return Outcome::transport_error(e.to_string());
                }
            }
        }
    };

    finish(&req, resp)
}

/// Format a fetch into an [`Outcome`]: write to stdout (`-O -`), an explicit file (`-O <file>`), or
/// a file named after the URL's basename (or a `Content-Disposition` header under
/// `--content-disposition`). A server error status (>= 400) is exit 4. `-S` prints the response
/// status line + headers to stderr.
fn finish(req: &Request, resp: whttp::Response) -> Outcome {
    let status = resp.status;
    let mut stderr = Vec::new();

    if req.server_response {
        stderr.extend_from_slice(server_response(&resp).as_bytes());
    }

    if status >= 400 {
        if !req.quiet {
            stderr.extend_from_slice(format!("waget: server returned status {status}\n").as_bytes());
        }
        return Outcome {
            stdout: Vec::new(),
            stderr,
            exit_code: 4,
        };
    }

    match &req.output {
        Output::Stdout => Outcome {
            stdout: resp.body,
            stderr,
            exit_code: 0,
        },
        Output::File(default_path) => {
            // `--content-disposition` renames only the URL-derived default, never an explicit `-O`.
            let target = if req.content_disposition && req.output_is_default {
                content_disposition_filename(&resp).unwrap_or_else(|| default_path.clone())
            } else {
                default_path.clone()
            };
            match std::fs::write(&target, &resp.body) {
                Ok(()) => {
                    if !req.quiet {
                        stderr.extend_from_slice(format!("waget: saved to {target}\n").as_bytes());
                    }
                    Outcome {
                        stdout: Vec::new(),
                        stderr,
                        exit_code: 0,
                    }
                }
                Err(e) => Outcome::transport_error(format!("cannot write {target}: {e}")),
            }
        }
    }
}

/// The `-S`/`--server-response` trace: the status line + response headers, indented like wget.
fn server_response(resp: &whttp::Response) -> String {
    let mut s = format!("  HTTP/1.1 {}\n", resp.status);
    for (k, v) in &resp.headers {
        s.push_str(&format!("  {k}: {v}\n"));
    }
    s
}

/// The filename from a `Content-Disposition: attachment; filename="…"` header, reduced to its
/// basename (guarding against a path-traversal filename).
fn content_disposition_filename(resp: &whttp::Response) -> Option<String> {
    let cd = resp.header("content-disposition")?;
    for part in cd.split(';') {
        let p = part.trim();
        if p.get(..9).is_some_and(|h| h.eq_ignore_ascii_case("filename=")) {
            let name = p[9..].trim().trim_matches('"');
            let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
            if !base.is_empty() {
                return Some(base.to_string());
            }
        }
    }
    None
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

    /// A one-shot server that adds a custom header (for `-S` / `--content-disposition`).
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

    /// A server that 302-redirects the first request to `/final`, then serves `body`.
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
    async fn follows_redirects_by_default() {
        let url = mock_redirect("final-body");
        let out = run(&argv(&["-O", "-", &url])).await;
        assert_eq!(out.exit_code, 0);
        assert_eq!(String::from_utf8(out.stdout).unwrap(), "final-body");
    }

    #[tokio::test]
    async fn server_response_prints_headers_to_stderr() {
        let url = mock_server_h(200, ("X-Trace", "abc"), "body");
        let out = run(&argv(&["-S", "-O", "-", &url])).await;
        let err = String::from_utf8(out.stderr).unwrap().to_lowercase();
        assert!(err.contains("http/1.1 200"), "status on stderr:\n{err}");
        assert!(err.contains("x-trace: abc"), "header on stderr:\n{err}");
    }

    #[test]
    fn content_disposition_filename_is_extracted_and_basenamed() {
        let resp = |cd: &str| whttp::Response {
            status: 200,
            headers: vec![("Content-Disposition".into(), cd.into())],
            body: Vec::new(),
            final_url: "http://x".into(),
        };
        assert_eq!(
            content_disposition_filename(&resp("attachment; filename=\"dl.txt\"")).as_deref(),
            Some("dl.txt")
        );
        assert_eq!(
            content_disposition_filename(&resp("inline; filename=report.pdf")).as_deref(),
            Some("report.pdf")
        );
        // Path-traversal filename is reduced to its basename.
        assert_eq!(
            content_disposition_filename(&resp("attachment; filename=\"/etc/passwd\"")).as_deref(),
            Some("passwd")
        );
        assert_eq!(content_disposition_filename(&resp("attachment")), None);
    }

    #[tokio::test]
    async fn post_data_completes() {
        let url = mock_server(200, "ok");
        let out = run(&argv(&["--post-data", "a=1", "-O", "-", &url])).await;
        assert_eq!(out.exit_code, 0);
        assert_eq!(String::from_utf8(out.stdout).unwrap(), "ok");
    }
}
