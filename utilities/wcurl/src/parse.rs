//! `curl` argument parsing.
//!
//! Supports a practical subset of curl: method/data/headers, redirect following (`-L`), response-
//! header output (`-i`/`-I`), fail-on-error (`-f`), timeouts (`-m`/`--connect-timeout`), write-out
//! (`-w`), auth and identity (`-u`/`-A`/`-e`), data-from-file (`-d @file`, `--data-binary`),
//! `--json`, query-from-data (`-G`), and verbose (`-v`). Short flags cluster (`-fsSL`) and long
//! flags take `--flag=value`. Unknown flags are an error so a typo isn't swallowed as a URL.

use http::Method;

/// A parsed `curl` invocation. (No `Eq`: the timeout fields are `f64`.)
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Request {
    pub url: String,
    pub method: Method,
    pub headers: Vec<(String, String)>,
    /// Request body (`-d`, `--data-binary`, `--json`), already resolved from `@file` if used.
    pub data: Option<Vec<u8>>,
    /// `-o <file>`: write the body here instead of stdout.
    pub output: Option<String>,
    /// `-s`/`--silent`: suppress the non-2xx status message on stderr.
    pub silent: bool,
    /// `-L`/`--location`: follow 3xx redirects.
    pub location: bool,
    /// `-i`/`--include`: prepend the response status line + headers to the output.
    pub include: bool,
    /// `-I`/`--head`: issue a HEAD request and print only the response headers.
    pub head: bool,
    /// `-f`/`--fail`: emit no body and exit non-zero on an HTTP error status (>= 400).
    pub fail: bool,
    /// `-m`/`--max-time` seconds: overall request time budget.
    pub max_time: Option<f64>,
    /// `--connect-timeout` seconds: connection time budget.
    pub connect_timeout: Option<f64>,
    /// `-w`/`--write-out` format string, expanded after the transfer.
    pub write_out: Option<String>,
    /// `-v`/`--verbose`: trace request/response headers to stderr.
    pub verbose: bool,
    /// `-G`/`--get`: send any `-d` data as the query string of a GET.
    pub get: bool,
}

/// A `curl` argument parsing error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseError {
    MissingUrl,
    MissingValue(String),
    UnknownFlag(String),
    BadMethod(String),
    BadNumber(String),
    BadData(String),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::MissingUrl => write!(f, "no URL specified"),
            ParseError::MissingValue(flag) => write!(f, "{flag}: option requires a value"),
            ParseError::UnknownFlag(flag) => write!(f, "unknown option: {flag}"),
            ParseError::BadMethod(m) => write!(f, "invalid method: {m}"),
            ParseError::BadNumber(v) => write!(f, "not a number: {v}"),
            ParseError::BadData(m) => write!(f, "{m}"),
        }
    }
}

/// Short flags that consume a value (`-o file`, and `-ofile` inside a cluster).
const VALUE_SHORTS: &[char] = &['o', 'X', 'd', 'H', 'm', 'w', 'A', 'u', 'e'];

/// Normalize argv into a flat flag/value stream: expand `--flag=value` into two tokens and split
/// short-flag clusters (`-fsSL` → `-f -s -S -L`), where a value-taking short consumes the rest of
/// its cluster as its value (`-ofile` → `-o file`) or, if nothing remains, the next argv token.
fn expand_args(args: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    for arg in args {
        if let Some(long) = arg.strip_prefix("--") {
            match long.split_once('=') {
                Some((flag, value)) => {
                    out.push(format!("--{flag}"));
                    out.push(value.to_string());
                }
                None => out.push(arg.clone()),
            }
        } else if arg.len() > 2 && arg.starts_with('-') && arg != "--" {
            // A short cluster like `-sL` or `-ofile`.
            let chars: Vec<char> = arg[1..].chars().collect();
            let mut i = 0;
            while i < chars.len() {
                let c = chars[i];
                out.push(format!("-{c}"));
                if VALUE_SHORTS.contains(&c) {
                    let rest: String = chars[i + 1..].iter().collect();
                    if !rest.is_empty() {
                        out.push(rest);
                    }
                    break;
                }
                i += 1;
            }
        } else {
            out.push(arg.clone());
        }
    }
    out
}

/// Parse argv (without the leading `curl` word) into a [`Request`].
pub(crate) fn parse(args: &[String]) -> Result<Request, ParseError> {
    let expanded = expand_args(args);

    let mut url: Option<String> = None;
    let mut method: Option<Method> = None;
    let mut headers = Vec::new();
    let mut data: Option<Vec<u8>> = None;
    let mut output = None;
    let mut silent = false;
    let mut location = false;
    let mut include = false;
    let mut head = false;
    let mut fail = false;
    let mut max_time = None;
    let mut connect_timeout = None;
    let mut write_out = None;
    let mut verbose = false;
    let mut get = false;
    let mut json = false;

    let mut iter = expanded.into_iter();
    while let Some(arg) = iter.next() {
        let mut next = |flag: &str| iter.next().ok_or_else(|| ParseError::MissingValue(flag.into()));
        match arg.as_str() {
            "-o" | "--output" => output = Some(next("-o")?),
            "-s" | "--silent" => silent = true,
            "-L" | "--location" => location = true,
            "-i" | "--include" => include = true,
            "-I" | "--head" => head = true,
            "-f" | "--fail" => fail = true,
            "-v" | "--verbose" => verbose = true,
            "-G" | "--get" => get = true,
            // `-S`/`--show-error` is accepted (it only matters alongside `-s`, which we already
            // honor by always surfacing transport errors); treat as a no-op rather than unknown.
            "-S" | "--show-error" => {}
            "-X" | "--request" => {
                let m = next("-X")?;
                method = Some(
                    Method::from_bytes(m.to_uppercase().as_bytes())
                        .map_err(|_| ParseError::BadMethod(m))?,
                );
            }
            "-d" | "--data" | "--data-binary" | "--data-raw" => {
                data = Some(resolve_data(&next("-d")?, arg == "--data-raw")?);
            }
            "--json" => {
                data = Some(resolve_data(&next("--json")?, false)?);
                json = true;
            }
            "-H" | "--header" => headers.push(split_header(&next("-H")?)),
            "-A" | "--user-agent" => headers.push(("User-Agent".to_string(), next("-A")?)),
            "-e" | "--referer" => headers.push(("Referer".to_string(), next("-e")?)),
            "-u" | "--user" => {
                let cred = next("-u")?;
                headers.push(("Authorization".to_string(), format!("Basic {}", base64(cred.as_bytes()))));
            }
            "-m" | "--max-time" => max_time = Some(parse_secs(&next("-m")?)?),
            "--connect-timeout" => connect_timeout = Some(parse_secs(&next("--connect-timeout")?)?),
            "-w" | "--write-out" => write_out = Some(next("-w")?),
            "--url" => url = Some(next("--url")?),
            other if other.starts_with('-') && other != "-" => {
                return Err(ParseError::UnknownFlag(other.to_string()));
            }
            _ if url.is_none() => url = Some(arg),
            other => return Err(ParseError::UnknownFlag(other.to_string())),
        }
    }

    // `--json` sets the content negotiation headers unless the caller already set them.
    if json {
        ensure_header(&mut headers, "Content-Type", "application/json");
        ensure_header(&mut headers, "Accept", "application/json");
    }

    // Method resolution: explicit `-X` wins; then `-I` (HEAD); then `-G` (GET); then a body implies
    // POST; else GET. whttp skips reading the (absent) body of a HEAD response — see `is_bodyless`.
    let method = method.unwrap_or_else(|| {
        if head {
            Method::HEAD
        } else if get {
            Method::GET
        } else if data.is_some() {
            Method::POST
        } else {
            Method::GET
        }
    });

    Ok(Request {
        url: url.ok_or(ParseError::MissingUrl)?,
        method,
        headers,
        data,
        output,
        silent,
        location,
        include,
        head,
        fail,
        max_time,
        connect_timeout,
        write_out,
        verbose,
        get,
    })
}

/// Resolve a `-d`/`--data`/`--json` value: a leading `@` (unless `--data-raw`) reads a file. `@-`
/// would mean stdin in curl; we don't have a stdin handle here, so it's an error.
fn resolve_data(value: &str, raw: bool) -> Result<Vec<u8>, ParseError> {
    if !raw {
        if let Some(path) = value.strip_prefix('@') {
            if path == "-" {
                return Err(ParseError::BadData("-d @-: stdin is not available here".into()));
            }
            return std::fs::read(path)
                .map_err(|e| ParseError::BadData(format!("cannot read {path}: {e}")));
        }
    }
    Ok(value.as_bytes().to_vec())
}

/// Add `(name, value)` only if no header with that name (case-insensitive) is present.
fn ensure_header(headers: &mut Vec<(String, String)>, name: &str, value: &str) {
    if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(name)) {
        headers.push((name.to_string(), value.to_string()));
    }
}

/// Parse a seconds value (curl accepts fractional seconds).
fn parse_secs(value: &str) -> Result<f64, ParseError> {
    value.parse::<f64>().map_err(|_| ParseError::BadNumber(value.to_string()))
}

/// Split a `-H "Key: Value"` argument into `(key, value)`, trimming whitespace. No colon → an
/// empty-valued header.
fn split_header(raw: &str) -> (String, String) {
    match raw.split_once(':') {
        Some((k, v)) => (k.trim().to_string(), v.trim().to_string()),
        None => (raw.trim().to_string(), String::new()),
    }
}

/// Standard base64 (with padding) — for `-u user:pass` → `Authorization: Basic …`. Hand-rolled to
/// avoid a dependency in this small wasm-facing crate.
fn base64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        let n = u32::from(b0) << 16 | u32::from(b1) << 8 | u32::from(b2);
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(std::string::ToString::to_string).collect()
    }

    #[test]
    fn plain_url_is_a_get() {
        let r = parse(&argv(&["https://example.com"])).unwrap();
        assert_eq!(r.url, "https://example.com");
        assert_eq!(r.method, Method::GET);
        assert!(!r.location && !r.include && !r.head && !r.fail);
    }

    #[test]
    fn data_implies_post_and_is_bytes() {
        let r = parse(&argv(&["-d", "a=1", "https://example.com"])).unwrap();
        assert_eq!(r.method, Method::POST);
        assert_eq!(r.data.as_deref(), Some(b"a=1".as_slice()));
    }

    #[test]
    fn explicit_method_overrides_data_default() {
        let r = parse(&argv(&["-X", "PUT", "-d", "a=1", "https://example.com"])).unwrap();
        assert_eq!(r.method, Method::PUT);
    }

    #[test]
    fn head_flag_sets_head_method() {
        let r = parse(&argv(&["-I", "https://example.com"])).unwrap();
        assert_eq!(r.method, Method::HEAD);
        assert!(r.head);
    }

    #[test]
    fn clustered_short_flags_expand() {
        // The classic installer one-liner.
        let r = parse(&argv(&["-fsSL", "https://example.com"])).unwrap();
        assert!(r.fail && r.silent && r.location);
    }

    #[test]
    fn clustered_value_short_takes_the_remainder() {
        let r = parse(&argv(&["-sod", "https://example.com"])).unwrap();
        // -s, then -o with value "d".
        assert!(r.silent);
        assert_eq!(r.output.as_deref(), Some("d"));
    }

    #[test]
    fn long_flag_equals_value() {
        let r = parse(&argv(&["--request=DELETE", "https://example.com"])).unwrap();
        assert_eq!(r.method, Method::DELETE);
    }

    #[test]
    fn json_sets_content_headers_and_posts() {
        let r = parse(&argv(&["--json", r#"{"a":1}"#, "https://example.com"])).unwrap();
        assert_eq!(r.method, Method::POST);
        assert!(r.headers.iter().any(|(k, v)| k == "Content-Type" && v == "application/json"));
        assert!(r.headers.iter().any(|(k, v)| k == "Accept" && v == "application/json"));
    }

    #[test]
    fn basic_auth_encodes_credentials() {
        let r = parse(&argv(&["-u", "user:pass", "https://example.com"])).unwrap();
        // base64("user:pass") == "dXNlcjpwYXNz"
        assert!(r.headers.iter().any(|(k, v)| k == "Authorization" && v == "Basic dXNlcjpwYXNz"));
    }

    #[test]
    fn user_agent_and_referer_become_headers() {
        let r = parse(&argv(&["-A", "clank/1", "-e", "https://ref", "https://example.com"])).unwrap();
        assert!(r.headers.iter().any(|(k, v)| k == "User-Agent" && v == "clank/1"));
        assert!(r.headers.iter().any(|(k, v)| k == "Referer" && v == "https://ref"));
    }

    #[test]
    fn timeouts_parse_as_seconds() {
        let r = parse(&argv(&["-m", "2.5", "--connect-timeout", "1", "https://example.com"])).unwrap();
        assert_eq!(r.max_time, Some(2.5));
        assert_eq!(r.connect_timeout, Some(1.0));
    }

    #[test]
    fn bad_timeout_is_an_error() {
        assert!(matches!(
            parse(&argv(&["-m", "soon", "https://x"])),
            Err(ParseError::BadNumber(_))
        ));
    }

    #[test]
    fn data_from_file_is_read() {
        let path = std::env::temp_dir().join(format!("wcurl_data_{}", std::process::id()));
        std::fs::write(&path, b"file-payload").unwrap();
        let r = parse(&argv(&["-d", &format!("@{}", path.display()), "https://x"])).unwrap();
        assert_eq!(r.data.as_deref(), Some(b"file-payload".as_slice()));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn data_raw_keeps_the_at_sign() {
        let r = parse(&argv(&["--data-raw", "@notafile", "https://x"])).unwrap();
        assert_eq!(r.data.as_deref(), Some(b"@notafile".as_slice()));
    }

    #[test]
    fn write_out_and_url_flags() {
        let r = parse(&argv(&["-w", "%{http_code}", "--url", "https://example.com"])).unwrap();
        assert_eq!(r.write_out.as_deref(), Some("%{http_code}"));
        assert_eq!(r.url, "https://example.com");
    }

    #[test]
    fn get_flag_records_intent() {
        let r = parse(&argv(&["-G", "-d", "q=1", "https://example.com"])).unwrap();
        assert!(r.get);
        assert_eq!(r.method, Method::GET);
    }

    #[test]
    fn missing_url_errors() {
        assert_eq!(parse(&argv(&["-s"])), Err(ParseError::MissingUrl));
    }

    #[test]
    fn unknown_flag_errors() {
        assert_eq!(
            parse(&argv(&["--bogus", "https://example.com"])),
            Err(ParseError::UnknownFlag("--bogus".into()))
        );
    }
}
