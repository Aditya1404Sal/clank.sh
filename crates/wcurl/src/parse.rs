//! `curl` argument parsing — the minimal flag set clank needs.
//!
//! Supported: `<url>` (positional), `-o <file>`, `-s`/`--silent`, `-X <METHOD>`, `-d <data>`
//! (implies POST if no `-X`), `-H "K: V"` (repeatable). Deliberately small; unknown flags are an
//! error so a typo isn't silently swallowed as a URL.

use http::Method;

/// A parsed `curl` invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub url: String,
    pub method: Method,
    pub headers: Vec<(String, String)>,
    pub data: Option<String>,
    /// `-o <file>`: write the body here instead of stdout.
    pub output: Option<String>,
    /// `-s`/`--silent`: suppress the non-2xx status message on stderr.
    pub silent: bool,
}

/// A `curl` argument parsing error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseError {
    MissingUrl,
    MissingValue(String),
    UnknownFlag(String),
    BadMethod(String),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::MissingUrl => write!(f, "no URL specified"),
            ParseError::MissingValue(flag) => write!(f, "{flag}: option requires a value"),
            ParseError::UnknownFlag(flag) => write!(f, "unknown option: {flag}"),
            ParseError::BadMethod(m) => write!(f, "invalid method: {m}"),
        }
    }
}

/// Parse argv (without the leading `curl` word) into a [`Request`].
pub fn parse(args: &[String]) -> Result<Request, ParseError> {
    let mut url: Option<String> = None;
    let mut method: Option<Method> = None;
    let mut headers = Vec::new();
    let mut data: Option<String> = None;
    let mut output: Option<String> = None;
    let mut silent = false;

    let mut iter = args.iter().cloned();
    while let Some(arg) = iter.next() {
        let mut next = |flag: &str| iter.next().ok_or_else(|| ParseError::MissingValue(flag.into()));
        match arg.as_str() {
            "-o" => output = Some(next("-o")?),
            "-s" | "--silent" => silent = true,
            "-X" | "--request" => {
                let m = next("-X")?;
                method = Some(
                    Method::from_bytes(m.to_uppercase().as_bytes())
                        .map_err(|_| ParseError::BadMethod(m))?,
                );
            }
            "-d" | "--data" => data = Some(next("-d")?),
            "-H" | "--header" => headers.push(split_header(&next("-H")?)),
            other if other.starts_with('-') && other != "-" => {
                return Err(ParseError::UnknownFlag(other.to_string()));
            }
            _ if url.is_none() => url = Some(arg),
            other => return Err(ParseError::UnknownFlag(other.to_string())),
        }
    }

    // `-d` implies POST unless a method was given explicitly (curl semantics).
    let method = method.unwrap_or_else(|| {
        if data.is_some() {
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
    })
}

/// Split a `-H "Key: Value"` argument into `(key, value)`, trimming surrounding whitespace. A header
/// with no colon becomes an empty-valued header (curl allows bare `-H "Key;"` to send an empty
/// header, but we keep it simple: no colon → empty value).
fn split_header(raw: &str) -> (String, String) {
    match raw.split_once(':') {
        Some((k, v)) => (k.trim().to_string(), v.trim().to_string()),
        None => (raw.trim().to_string(), String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn plain_url_is_a_get() {
        let r = parse(&argv(&["https://example.com"])).unwrap();
        assert_eq!(r.url, "https://example.com");
        assert_eq!(r.method, Method::GET);
        assert!(r.headers.is_empty() && r.data.is_none() && r.output.is_none() && !r.silent);
    }

    #[test]
    fn output_and_silent_flags() {
        let r = parse(&argv(&["-s", "-o", "/tmp/x", "https://example.com"])).unwrap();
        assert!(r.silent);
        assert_eq!(r.output.as_deref(), Some("/tmp/x"));
    }

    #[test]
    fn data_implies_post() {
        let r = parse(&argv(&["-d", "a=1", "https://example.com"])).unwrap();
        assert_eq!(r.method, Method::POST);
        assert_eq!(r.data.as_deref(), Some("a=1"));
    }

    #[test]
    fn explicit_method_overrides_data_default() {
        let r = parse(&argv(&["-X", "PUT", "-d", "a=1", "https://example.com"])).unwrap();
        assert_eq!(r.method, Method::PUT);
    }

    #[test]
    fn headers_are_split_and_trimmed() {
        let r = parse(&argv(&["-H", "Accept: application/json", "https://example.com"])).unwrap();
        assert_eq!(
            r.headers,
            vec![("Accept".to_string(), "application/json".to_string())]
        );
    }

    #[test]
    fn missing_url_errors() {
        assert_eq!(parse(&argv(&["-s"])), Err(ParseError::MissingUrl));
    }

    #[test]
    fn missing_flag_value_errors() {
        assert_eq!(
            parse(&argv(&["-o"])),
            Err(ParseError::MissingValue("-o".into()))
        );
    }

    #[test]
    fn unknown_flag_errors() {
        assert_eq!(
            parse(&argv(&["--bogus", "https://example.com"])),
            Err(ParseError::UnknownFlag("--bogus".into()))
        );
    }

    #[test]
    fn bad_method_errors() {
        assert!(matches!(
            parse(&argv(&["-X", "b a d", "https://example.com"])),
            Err(ParseError::BadMethod(_))
        ));
    }
}
