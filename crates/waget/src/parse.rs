//! `wget` argument parsing — the minimal flag set clank needs.
//!
//! Supported: `<url>` (positional), `-O <file>` (`-O -` = stdout), `-q`/`--quiet`. GET only.

/// Where a fetched body is written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Output {
    /// Stream to stdout (`-O -`).
    Stdout,
    /// Write to this path (explicit `-O <file>`, or the default basename).
    File(String),
}

/// A parsed `wget` invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub url: String,
    pub output: Output,
    /// `-q`/`--quiet`: suppress the progress/saved/status messages on stderr.
    pub quiet: bool,
}

/// A `wget` argument parsing error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseError {
    MissingUrl,
    MissingValue(String),
    UnknownFlag(String),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::MissingUrl => write!(f, "no URL specified"),
            ParseError::MissingValue(flag) => write!(f, "{flag}: option requires a value"),
            ParseError::UnknownFlag(flag) => write!(f, "unknown option: {flag}"),
        }
    }
}

/// Parse argv (without the leading `wget` word) into a [`Request`].
pub fn parse(args: &[String]) -> Result<Request, ParseError> {
    let mut url: Option<String> = None;
    let mut explicit_output: Option<Output> = None;
    let mut quiet = false;

    let mut iter = args.iter().cloned();
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
            other if other.starts_with('-') && other != "-" => {
                return Err(ParseError::UnknownFlag(other.to_string()));
            }
            _ if url.is_none() => url = Some(arg),
            other => return Err(ParseError::UnknownFlag(other.to_string())),
        }
    }

    let url = url.ok_or(ParseError::MissingUrl)?;
    // Default output: a file named after the URL's last path segment (wget's behavior), falling back
    // to `index.html` when the URL has no filename component.
    let output = explicit_output.unwrap_or_else(|| Output::File(default_filename(&url)));

    Ok(Request { url, output, quiet })
}

/// The default output filename for a URL: its last non-empty path segment (stripped of any query),
/// or `index.html` if there is none.
fn default_filename(url: &str) -> String {
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
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn plain_url_defaults_to_basename_file() {
        let r = parse(&argv(&["https://example.com/dir/report.txt"])).unwrap();
        assert_eq!(r.output, Output::File("report.txt".to_string()));
        assert!(!r.quiet);
    }

    #[test]
    fn host_only_url_defaults_to_index_html() {
        let r = parse(&argv(&["https://example.com"])).unwrap();
        assert_eq!(r.output, Output::File("index.html".to_string()));
    }

    #[test]
    fn dash_o_dash_is_stdout() {
        let r = parse(&argv(&["-O", "-", "https://example.com/f"])).unwrap();
        assert_eq!(r.output, Output::Stdout);
    }

    #[test]
    fn dash_o_names_a_file() {
        let r = parse(&argv(&["-O", "out.bin", "https://example.com/f"])).unwrap();
        assert_eq!(r.output, Output::File("out.bin".to_string()));
    }

    #[test]
    fn quiet_flag() {
        let r = parse(&argv(&["-q", "https://example.com/f"])).unwrap();
        assert!(r.quiet);
    }

    #[test]
    fn missing_url_errors() {
        assert_eq!(parse(&argv(&["-q"])), Err(ParseError::MissingUrl));
    }

    #[test]
    fn missing_output_value_errors() {
        assert_eq!(
            parse(&argv(&["-O"])),
            Err(ParseError::MissingValue("-O".into()))
        );
    }

    #[test]
    fn unknown_flag_errors() {
        assert_eq!(
            parse(&argv(&["--bogus", "https://example.com"])),
            Err(ParseError::UnknownFlag("--bogus".into()))
        );
    }
}
