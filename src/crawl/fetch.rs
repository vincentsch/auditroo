use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use url::Url;

/// Fetches one URL. The robots.txt loader and the crawl driver use it behind
/// `Arc<dyn Fetcher>`, so the HTTP and the browser fetcher can stand in for
/// each other.
#[async_trait]
pub trait Fetcher: Send + Sync {
    /// Fetch `url`. Each fetcher follows redirects in its own way.
    // async_trait adds a bare #[must_use] to this boxed Future declaration.
    // Rust 1.99 recognizes the Future itself as must-use; the generated
    // attribute cannot be removed at the call site.
    #[allow(clippy::double_must_use)]
    async fn fetch(&self, url: &Url) -> Result<FetchedResponse, FetchError>;
}

/// One redirect that was followed while fetching a URL.
///
/// `from` and `to` are full URLs as the `url` crate writes them, not the raw
/// `Location` header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedirectHop {
    pub from: String,
    pub status: u16,
    pub to: String,
}

/// Response headers in a form that serializes the same way every time.
///
/// Names are stored in lowercase, so lookups ignore case. A header that comes
/// more than once keeps all its values in the order they arrived.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ResponseHeaders {
    inner: BTreeMap<String, Vec<String>>,
}

impl ResponseHeaders {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build headers from `(name, value)` pairs. Repeated names collect all
    /// their values.
    pub fn from_pairs<I, K, V>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: Into<String>,
    {
        let mut headers = Self::new();
        for (name, value) in pairs {
            headers.push(name.as_ref(), value);
        }
        headers
    }

    pub fn push(&mut self, name: &str, value: impl Into<String>) {
        self.inner
            .entry(name.to_ascii_lowercase())
            .or_default()
            .push(value.into());
    }

    pub fn get_all_case_insensitive(&self, name: &str) -> &[String] {
        self.inner
            .get(&name.to_ascii_lowercase())
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Header names in lowercase, in sorted order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &[String])> {
        self.inner
            .iter()
            .map(|(name, values)| (name.as_str(), values.as_slice()))
    }
}

/// What a `Fetcher` got back for one URL.
///
/// A 404 or 500 is a normal response here. `FetchError` is only for requests
/// that got no usable response at all, such as a timeout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchedResponse {
    /// The URL after all redirects.
    pub final_url: String,
    /// The redirects that were followed, in order.
    pub redirects: Vec<RedirectHop>,
    /// The status of the last response, 4xx and 5xx included.
    pub status: u16,
    pub headers: ResponseHeaders,
    /// Media type in lowercase without parameters, for example `text/html`.
    pub content_type: Option<String>,
    /// The body as text, decoded with the charset the server names, or as
    /// UTF-8 when it names none.
    pub body: String,
    /// `true` when the page was rendered in the browser.
    pub rendered: bool,
}

/// A response with its raw bytes, for sitemaps.
///
/// Sitemaps can be gzipped, and the XML parser wants the original bytes.
/// Pages use `FetchedResponse`, so the crawl does not hold on to raw bytes
/// after it has decoded them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchedBytesResponse {
    pub final_url: String,
    pub redirects: Vec<RedirectHop>,
    pub status: u16,
    pub headers: ResponseHeaders,
    /// Media type in lowercase without parameters, for example `application/xml`.
    pub content_type: Option<String>,
    /// The raw body, at most `max_body_bytes` long.
    pub bytes: Vec<u8>,
}

/// A request that got no usable response.
///
/// The variants hold plain strings instead of reqwest's error types, so the
/// browser fetcher can return the same errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    /// A header from `--header` or `[crawl] headers` is not a valid header.
    InvalidHeader {
        name: String,
        detail: String,
    },
    /// `--header` tried to set the User-Agent.
    UserAgentHeaderNotAllowed {
        name: String,
    },
    /// The User-Agent is not a valid header value.
    InvalidUserAgent {
        detail: String,
    },
    /// The HTTP client or the browser could not be set up. The detail says
    /// which.
    ClientBuild {
        detail: String,
    },
    /// Any other failed request, for example DNS or a refused connection.
    Request {
        detail: String,
    },
    Timeout,
    /// A redirect points somewhere auditroo cannot follow.
    MalformedRedirect {
        detail: String,
    },
    /// The response is larger than `max_body_bytes`.
    BodyTooLarge {
        limit: usize,
    },
    /// The browser stopped working. The rest of the crawl cannot use it.
    BackendUnavailable {
        reason: BackendUnavailableReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendUnavailableReason {
    ProcessExited,
    CdpDisconnected,
    HandlerFailed,
    HandlerEnded,
    HandlerPanicked,
    AlreadyUnavailable,
    ShutdownFailed,
}

impl BackendUnavailableReason {
    /// What happened to the browser, for messages.
    pub fn describe(self) -> &'static str {
        match self {
            Self::ProcessExited => "Chrome exited",
            Self::CdpDisconnected => "the connection to Chrome was lost",
            Self::HandlerFailed | Self::HandlerEnded | Self::HandlerPanicked => {
                "auditroo lost track of Chrome's events"
            }
            Self::AlreadyUnavailable => "Chrome had already stopped",
            Self::ShutdownFailed => "Chrome did not shut down cleanly",
        }
    }
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FetchError::InvalidHeader { name, detail } => {
                write!(
                    f,
                    "the header {name:?} is not a valid HTTP header ({detail})"
                )
            }
            FetchError::UserAgentHeaderNotAllowed { name } => {
                write!(
                    f,
                    "{name:?} cannot be set with --header. Use --user-agent or user_agent in [crawl]"
                )
            }
            FetchError::InvalidUserAgent { detail } => {
                write!(f, "the User-Agent is not a valid header value ({detail})")
            }
            FetchError::ClientBuild { detail } => f.write_str(detail),
            FetchError::Request { detail } => f.write_str(detail),
            FetchError::Timeout => write!(f, "the request timed out"),
            FetchError::MalformedRedirect { detail } => {
                write!(f, "the redirect cannot be followed: {detail}")
            }
            FetchError::BodyTooLarge { limit } => {
                write!(
                    f,
                    "the response is larger than {limit} bytes (--max-body-bytes)"
                )
            }
            FetchError::BackendUnavailable { reason } => {
                write!(f, "the browser stopped working ({})", reason.describe())
            }
        }
    }
}

impl Error for FetchError {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn response_headers_lowercase_and_preserve_repeated_values() {
        let mut headers = ResponseHeaders::new();
        headers.push("X-Robots-Tag", "noindex");
        headers.push("x-robots-tag", "nofollow");
        headers.push("Content-Type", "text/html");

        assert_eq!(
            headers.get_all_case_insensitive("X-ROBOTS-TAG"),
            ["noindex", "nofollow"]
        );
        assert_eq!(
            headers.get_all_case_insensitive("content-type"),
            ["text/html"]
        );
        assert!(headers.get_all_case_insensitive("missing").is_empty());

        let iterated = headers
            .iter()
            .map(|(name, values)| (name.to_owned(), values.to_vec()))
            .collect::<Vec<_>>();
        assert_eq!(
            iterated,
            vec![
                ("content-type".to_owned(), vec!["text/html".to_owned()]),
                (
                    "x-robots-tag".to_owned(),
                    vec!["noindex".to_owned(), "nofollow".to_owned()]
                ),
            ]
        );
    }

    #[test]
    fn response_headers_from_pairs_merges_case_insensitively() {
        let headers = ResponseHeaders::from_pairs([
            ("X-Robots-Tag", "noindex"),
            ("x-robots-tag", "nofollow"),
        ]);

        assert_eq!(
            headers.get_all_case_insensitive("x-robots-tag"),
            ["noindex", "nofollow"]
        );
    }

    #[test]
    fn response_headers_serde_is_transparent() {
        let headers = ResponseHeaders::from_pairs([
            ("X-Robots-Tag", "noindex"),
            ("x-robots-tag", "nofollow"),
        ]);

        let value = serde_json::to_value(headers).expect("headers serialize");
        assert_eq!(
            value,
            json!({
                "x-robots-tag": ["noindex", "nofollow"]
            })
        );
        assert!(value.get("inner").is_none());
    }

    #[test]
    fn fetched_response_round_trips_through_json() {
        let response = FetchedResponse {
            final_url: "https://example.com/b".to_owned(),
            redirects: vec![RedirectHop {
                from: "https://example.com/a".to_owned(),
                status: 301,
                to: "https://example.com/b".to_owned(),
            }],
            status: 200,
            headers: ResponseHeaders::from_pairs([("Content-Type", "text/html")]),
            content_type: Some("text/html".to_owned()),
            body: "<html></html>".to_owned(),
            rendered: false,
        };

        let encoded = serde_json::to_string(&response).expect("response serializes");
        let decoded: FetchedResponse =
            serde_json::from_str(&encoded).expect("response deserializes");

        assert_eq!(decoded, response);
    }

    #[test]
    fn fetched_bytes_response_round_trips_through_json() {
        let response = FetchedBytesResponse {
            final_url: "https://example.com/sitemap.xml".to_owned(),
            redirects: vec![RedirectHop {
                from: "https://example.com/sitemap".to_owned(),
                status: 301,
                to: "https://example.com/sitemap.xml".to_owned(),
            }],
            status: 200,
            headers: ResponseHeaders::from_pairs([("Content-Type", "application/xml")]),
            content_type: Some("application/xml".to_owned()),
            bytes: b"<urlset/>".to_vec(),
        };

        let encoded = serde_json::to_string(&response).expect("byte response serializes");
        let decoded: FetchedBytesResponse =
            serde_json::from_str(&encoded).expect("byte response deserializes");

        assert_eq!(decoded, response);
    }
}
