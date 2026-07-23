//! `wget` argument parsing.
//!
//! Supports a practical subset of wget: output selection (`-O`), quiet (`-q`), redirect following
//! (on by default, capped by `--max-redirect`), response-header printing (`-S`), timeout (`-T`),
//! retries (`-t`), POST (`--post-data`/`--post-file`), extra headers (`--header`), user-agent
//! (`-U`), and `--content-disposition` naming. `--flag=value` is accepted. A few no-op-safe flags
//! (`--no-check-certificate`, `-nv`, `-v`) are accepted so common command lines don't error.

use http::Method;

/// Where a fetched body is written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Output {
    /// Stream to stdout (`-O -`).
    Stdout,
    /// Write to this path (explicit `-O <file>`, or the default basename).
    File(String),
}

/// A parsed `wget` invocation.
#[derive(Clone, Debug, PartialEq)]
// independent wget flag toggles, not a state enum
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct Request {
    pub url: String,
    pub output: Output,
    /// True when `output` is the URL-derived default (no explicit `-O`) — the only case
    /// `--content-disposition` may rename.
    pub output_is_default: bool,
    /// `-q`/`--quiet`: suppress the progress/saved/status messages on stderr.
    pub quiet: bool,
    pub method: Method,
    pub headers: Vec<(String, String)>,
    /// `--post-data`/`--post-file` body.
    pub data: Option<Vec<u8>>,
    /// Follow 3xx redirects (wget does by default), capped by `max_redirect`.
    pub follow_redirects: bool,
    pub max_redirect: u32,
    /// `-S`/`--server-response`: print the response status line + headers to stderr.
    pub server_response: bool,
    /// `-T`/`--timeout` seconds: applied to both connect and read.
    pub timeout: Option<f64>,
    /// `-t`/`--tries`: total attempts on transport failure (default 1).
    pub tries: u32,
    /// `--content-disposition`: name the default output from a `Content-Disposition` header.
    pub content_disposition: bool,
}

/// A `wget` argument parsing error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// No URL argument was given.
    MissingUrl,
    /// A flag that requires a value was given none (holds the flag name).
    MissingValue(String),
    /// An unrecognized option was encountered (holds the offending flag).
    UnknownFlag(String),
    /// A numeric flag value (`-T`/`-t`/`--max-redirect`) failed to parse (holds the bad text).
    BadNumber(String),
    /// A `--post-file` body could not be read (holds the error message).
    BadData(String),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::MissingUrl => write!(f, "no URL specified"),
            ParseError::MissingValue(flag) => write!(f, "{flag}: option requires a value"),
            ParseError::UnknownFlag(flag) => write!(f, "unknown option: {flag}"),
            ParseError::BadNumber(v) => write!(f, "not a number: {v}"),
            ParseError::BadData(m) => write!(f, "{m}"),
        }
    }
}

/// Expand `--flag=value` into two tokens. (Unlike curl, wget short flags are matched whole — no
/// clustering — so `-nv`/`-nc` stay single options.)
fn expand_args(args: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    for arg in args {
        match arg.strip_prefix("--").and_then(|l| l.split_once('=')) {
            Some((flag, value)) => {
                out.push(format!("--{flag}"));
                out.push(value.to_string());
            }
            None => out.push(arg.clone()),
        }
    }
    out
}

/// Parse argv (without the leading `wget` word) into a [`Request`].
pub(crate) fn parse(args: &[String]) -> Result<Request, ParseError> {
    let expanded = expand_args(args);

    let mut url: Option<String> = None;
    let mut explicit_output: Option<Output> = None;
    let mut quiet = false;
    let mut headers = Vec::new();
    let mut data: Option<Vec<u8>> = None;
    let mut server_response = false;
    let mut timeout = None;
    let mut tries = 1u32;
    let mut max_redirect = 20u32;
    let mut content_disposition = false;

    let mut iter = expanded.into_iter();
    while let Some(arg) = iter.next() {
        let mut next = |flag: &str| iter.next().ok_or_else(|| ParseError::MissingValue(flag.into()));
        match arg.as_str() {
            "-O" | "--output-document" => {
                let target = next("-O")?;
                explicit_output = Some(if target == "-" {
                    Output::Stdout
                } else {
                    Output::File(target)
                });
            }
            "-q" | "--quiet" => quiet = true,
            "-S" | "--server-response" => server_response = true,
            "-T" | "--timeout" => timeout = Some(parse_secs(&next("-T")?)?),
            "-t" | "--tries" => tries = parse_count(&next("-t")?)?.max(1),
            "--max-redirect" => max_redirect = parse_count(&next("--max-redirect")?)?,
            "--content-disposition" => content_disposition = true,
            "-U" | "--user-agent" => headers.push(("User-Agent".to_string(), next("-U")?)),
            "--header" => headers.push(split_header(&next("--header")?)),
            "--post-data" => data = Some(next("--post-data")?.into_bytes()),
            "--post-file" => {
                let path = next("--post-file")?;
                data = Some(
                    std::fs::read(&path).map_err(|e| ParseError::BadData(format!("cannot read {path}: {e}")))?,
                );
            }
            // Accepted no-ops so common command lines don't error. `--no-check-certificate` does NOT
            // actually disable verification (WASI-HTTP doesn't expose that) — it is honored as "don't
            // fail parsing", the safer interpretation.
            "--no-check-certificate" | "-nv" | "--no-verbose" | "-v" | "--verbose" => {}
            other if other.starts_with('-') && other != "-" => {
                return Err(ParseError::UnknownFlag(other.to_string()));
            }
            _ if url.is_none() => url = Some(arg),
            other => return Err(ParseError::UnknownFlag(other.to_string())),
        }
    }

    let url = url.ok_or(ParseError::MissingUrl)?;
    let (output, output_is_default) = match explicit_output {
        Some(o) => (o, false),
        None => (Output::File(default_filename(&url)), true),
    };
    let method = if data.is_some() { Method::POST } else { Method::GET };

    Ok(Request {
        url,
        output,
        output_is_default,
        quiet,
        method,
        headers,
        data,
        follow_redirects: true,
        max_redirect,
        server_response,
        timeout,
        tries,
        content_disposition,
    })
}

fn parse_secs(value: &str) -> Result<f64, ParseError> {
    value.parse::<f64>().map_err(|_| ParseError::BadNumber(value.to_string()))
}

fn parse_count(value: &str) -> Result<u32, ParseError> {
    value.parse::<u32>().map_err(|_| ParseError::BadNumber(value.to_string()))
}

/// Split a `--header "Key: Value"` argument into `(key, value)`, trimming whitespace.
fn split_header(raw: &str) -> (String, String) {
    match raw.split_once(':') {
        Some((k, v)) => (k.trim().to_string(), v.trim().to_string()),
        None => (raw.trim().to_string(), String::new()),
    }
}

/// The default output filename for a URL: its last non-empty path segment (stripped of any query),
/// or `index.html` if there is none.
pub(crate) fn default_filename(url: &str) -> String {
    let without_scheme = url.split("://").nth(1).unwrap_or(url);
    let path = without_scheme.split(['?', '#']).next().unwrap_or("");
    match path.rsplit('/').find(|seg| !seg.is_empty()) {
        // The first path segment is the host; treat a host-only URL as having no filename.
        Some(seg) if path.contains('/') => seg.to_string(),
        _ => "index.html".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(std::string::ToString::to_string).collect()
    }

    #[test]
    fn plain_url_defaults_to_basename_file() {
        let r = parse(&argv(&["https://example.com/dir/report.txt"])).unwrap();
        assert_eq!(r.output, Output::File("report.txt".to_string()));
        assert!(r.output_is_default);
        assert!(r.follow_redirects, "wget follows redirects by default");
        assert_eq!(r.method, Method::GET);
        assert_eq!(r.tries, 1);
    }

    #[test]
    fn dash_o_is_explicit_not_default() {
        let r = parse(&argv(&["-O", "out.bin", "https://example.com/f"])).unwrap();
        assert_eq!(r.output, Output::File("out.bin".to_string()));
        assert!(!r.output_is_default);
    }

    #[test]
    fn post_data_sets_post_and_body() {
        let r = parse(&argv(&["--post-data", "a=1&b=2", "https://example.com"])).unwrap();
        assert_eq!(r.method, Method::POST);
        assert_eq!(r.data.as_deref(), Some(b"a=1&b=2".as_slice()));
    }

    #[test]
    fn post_file_reads_body() {
        let path = std::env::temp_dir().join(format!("waget_post_{}", std::process::id()));
        std::fs::write(&path, b"payload").unwrap();
        let r = parse(&argv(&["--post-file", path.to_str().unwrap(), "https://x"])).unwrap();
        assert_eq!(r.method, Method::POST);
        assert_eq!(r.data.as_deref(), Some(b"payload".as_slice()));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn header_and_user_agent() {
        let r = parse(&argv(&[
            "--header=Accept: text/plain",
            "-U",
            "clank/1",
            "https://x",
        ]))
        .unwrap();
        assert!(r.headers.iter().any(|(k, v)| k == "Accept" && v == "text/plain"));
        assert!(r.headers.iter().any(|(k, v)| k == "User-Agent" && v == "clank/1"));
    }

    #[test]
    fn timeout_tries_and_max_redirect() {
        let r = parse(&argv(&["-T", "3", "-t", "5", "--max-redirect", "2", "https://x"])).unwrap();
        assert_eq!(r.timeout, Some(3.0));
        assert_eq!(r.tries, 5);
        assert_eq!(r.max_redirect, 2);
    }

    #[test]
    fn server_response_and_content_disposition_flags() {
        let r = parse(&argv(&["-S", "--content-disposition", "https://x/f"])).unwrap();
        assert!(r.server_response);
        assert!(r.content_disposition);
    }

    #[test]
    fn accepted_no_ops_stay_no_ops() {
        // Audit P3-6: assert the accepted no-op flags didn't error AND didn't flip real state — a
        // regression that made `-nv`/`--no-check-certificate` consume the URL or set quiet/
        // server_response would still have passed the old `.is_ok()`-only check.
        let r = parse(&argv(&["--no-check-certificate", "-nv", "https://x/f"])).unwrap();
        assert_eq!(r.url, "https://x/f");
        assert!(!r.quiet, "-nv must not set quiet (that is -q)");
        assert!(!r.server_response, "the no-ops must not set server_response (that is -S)");
    }

    #[test]
    fn unknown_flag_errors() {
        assert_eq!(
            parse(&argv(&["--bogus", "https://example.com"])),
            Err(ParseError::UnknownFlag("--bogus".into()))
        );
    }

    #[test]
    fn missing_url_errors() {
        assert_eq!(parse(&argv(&["-q"])), Err(ParseError::MissingUrl));
    }
}
