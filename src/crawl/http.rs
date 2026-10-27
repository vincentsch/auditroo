use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{HeaderName, HeaderValue, CONTENT_TYPE, LOCATION};
use reqwest::redirect::Policy;
use reqwest::Client;
use url::Url;

use crate::crawl::fetch::{
    FetchError, FetchedBytesResponse, FetchedResponse, Fetcher, RedirectHop, ResponseHeaders,
};

const REPOSITORY_URL: &str = "https://github.com/vincentsch/auditroo";

/// The User-Agent auditroo sends unless the user sets another one.
pub(crate) fn default_user_agent() -> String {
    format!(
        "auditroo/{} (+{})",
        env!("CARGO_PKG_VERSION"),
        REPOSITORY_URL
    )
}

/// Settings for the HTTP fetcher.
///
/// The timeout applies to each request in a redirect chain, not to the whole
/// chain. Custom headers are the user's own, for example a token that lets
/// auditroo through a firewall. `HttpFetcher::new` checks them, and each
/// request decides whether it may carry them.
#[derive(Clone)]
pub struct HttpFetcherConfig {
    /// `None` sends `auditroo/<version> (+repo)`.
    pub user_agent: Option<String>,
    /// Custom headers, already split into name and value.
    pub headers: Vec<(String, String)>,
    /// The only host that gets the custom headers. The commands set it to
    /// the start URL. Without it, the host of the requested URL gets them.
    pub header_origin: Option<Url>,
    /// Timeout for each request, in milliseconds.
    pub timeout_ms: u64,
    /// Larger bodies are a `BodyTooLarge` error.
    pub max_body_bytes: usize,
    /// How many redirects to follow at most.
    pub max_redirects: usize,
}

impl fmt::Debug for HttpFetcherConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let redacted_headers: Vec<_> = self
            .headers
            .iter()
            .map(|(name, _)| (name.as_str(), "[REDACTED]"))
            .collect();

        formatter
            .debug_struct("HttpFetcherConfig")
            .field("user_agent", &self.user_agent)
            .field("headers", &redacted_headers)
            .field("header_origin", &self.header_origin)
            .field("timeout_ms", &self.timeout_ms)
            .field("max_body_bytes", &self.max_body_bytes)
            .field("max_redirects", &self.max_redirects)
            .finish()
    }
}

impl Default for HttpFetcherConfig {
    fn default() -> Self {
        Self {
            user_agent: None,
            headers: Vec::new(),
            header_origin: None,
            timeout_ms: 10_000,
            max_body_bytes: 10_485_760,
            max_redirects: 10,
        }
    }
}

impl HttpFetcherConfig {
    /// Split a `"Name: Value"` line. The tests use it; the commands parse
    /// `--header` in the config module, which also checks the header.
    pub fn header_from_line(line: &str) -> Option<(String, String)> {
        let (name, value) = line.split_once(':')?;
        Some((name.trim().to_owned(), value.trim().to_owned()))
    }
}

/// The `Fetcher` for plain HTTP, built on reqwest.
///
/// auditroo follows redirects itself instead of letting reqwest do it, so it
/// can record each hop. The User-Agent goes with every request. Custom
/// headers only go to the host in `header_origin`, so a redirect to another
/// host never sees them.
pub struct HttpFetcher {
    client: Client,
    custom_headers: Vec<(HeaderName, HeaderValue)>,
    header_origin: Option<Url>,
    timeout_ms: u64,
    max_body_bytes: usize,
    max_redirects: usize,
}

impl HttpFetcher {
    /// Check the headers and build the HTTP client. Nothing goes over the
    /// network yet, so a bad header fails here and not halfway through a
    /// crawl.
    pub fn new(config: HttpFetcherConfig) -> Result<Self, FetchError> {
        let user_agent = config.user_agent.unwrap_or_else(default_user_agent);
        let user_agent =
            HeaderValue::from_str(&user_agent).map_err(|error| FetchError::InvalidUserAgent {
                detail: error.to_string(),
            })?;

        let mut custom_headers = Vec::with_capacity(config.headers.len());
        for (name, value) in config.headers {
            if name.eq_ignore_ascii_case("user-agent") {
                return Err(FetchError::UserAgentHeaderNotAllowed { name });
            }

            let header_name = HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
                FetchError::InvalidHeader {
                    name: name.clone(),
                    detail: error.to_string(),
                }
            })?;
            let header_value =
                HeaderValue::from_str(&value).map_err(|error| FetchError::InvalidHeader {
                    name: name.clone(),
                    detail: error.to_string(),
                })?;
            custom_headers.push((header_name, header_value));
        }

        let client = Client::builder()
            .redirect(Policy::none())
            .user_agent(user_agent)
            .build()
            .map_err(|error| FetchError::ClientBuild {
                detail: format!("could not set up the HTTP client: {error}"),
            })?;

        Ok(Self {
            client,
            custom_headers,
            header_origin: config.header_origin,
            timeout_ms: config.timeout_ms,
            max_body_bytes: config.max_body_bytes,
            max_redirects: config.max_redirects,
        })
    }

    /// Fetch a sitemap as raw bytes. Custom headers only go to the host of
    /// `header_origin`, the start URL.
    pub async fn fetch_bytes(
        &self,
        url: &Url,
        header_origin: &Url,
    ) -> Result<FetchedBytesResponse, FetchError> {
        self.fetch_bytes_with_observation(url, header_origin)
            .await
            .map_err(|failure| failure.error)
    }

    pub(crate) async fn fetch_bytes_with_observation(
        &self,
        url: &Url,
        header_origin: &Url,
    ) -> Result<FetchedBytesResponse, FetchedBytesFailure> {
        let terminal = self.fetch_terminal_response(url, header_origin).await?;
        let final_url = terminal.current.to_string();
        let final_status = terminal.status;

        self.build_bytes_response(terminal)
            .await
            .map_err(|error| FetchedBytesFailure {
                error,
                final_url: Some(final_url),
                final_status: Some(final_status),
            })
    }

    async fn fetch_terminal_response(
        &self,
        url: &Url,
        header_origin: &Url,
    ) -> Result<TerminalResponse, FetchedBytesFailure> {
        let mut current = url.clone();
        let mut redirects = Vec::new();

        loop {
            // One request per redirect hop, each with its own timeout.
            let mut request = self
                .client
                .get(current.clone())
                .timeout(Duration::from_millis(self.timeout_ms));

            // Custom headers only go to the host of `header_origin`, which is
            // the start URL in every command. A sitemap index can point to
            // sitemaps on other hosts, and those must not get the user's
            // tokens.
            if custom_header_target_matches_origin(header_origin, &current) {
                for (name, value) in &self.custom_headers {
                    request = request.header(name.clone(), value.clone());
                }
            }

            let response = request
                .send()
                .await
                .map_err(|error| FetchedBytesFailure::from(map_send_error(error)))?;
            let status = response.status();
            let status_code = status.as_u16();

            if status.is_redirection() {
                if let Some(location) = response.headers().get(LOCATION) {
                    if redirects.len() < self.max_redirects {
                        // A relative Location is relative to the URL that
                        // redirected.
                        let location = location.to_str().map_err(|error| {
                            observed_fetch_failure(
                                &current,
                                status_code,
                                FetchError::MalformedRedirect {
                                    detail: error.to_string(),
                                },
                            )
                        })?;
                        let target = current.join(location).map_err(|error| {
                            observed_fetch_failure(
                                &current,
                                status_code,
                                FetchError::MalformedRedirect {
                                    detail: error.to_string(),
                                },
                            )
                        })?;
                        if !matches!(target.scheme(), "http" | "https") {
                            return Err(observed_fetch_failure(
                                &current,
                                status_code,
                                FetchError::MalformedRedirect {
                                    detail: "it points to a URL that is not http:// or https://"
                                        .to_owned(),
                                },
                            ));
                        }

                        redirects.push(RedirectHop {
                            from: current.to_string(),
                            status: status_code,
                            to: target.to_string(),
                        });
                        current = target;
                        continue;
                    }
                }
            }

            // Not a redirect, a redirect without Location, or the redirect
            // limit is reached: this is the final response. At the limit the
            // last Location is not followed; it stays in the headers.
            return Ok(TerminalResponse {
                current,
                redirects,
                status: status_code,
                response,
            });
        }
    }

    async fn build_response(
        &self,
        terminal: TerminalResponse,
    ) -> Result<FetchedResponse, FetchError> {
        let ResponseMetadata {
            headers,
            content_type,
            charset,
        } = response_metadata(&terminal.response);
        // Pages, robots.txt and other text are read. Images, PDFs and other
        // binary files are not: the crawl only needs their status code.
        let body = if content_type.as_deref().is_none_or(is_text_media_type) {
            self.read_capped_bytes(terminal.response).await?
        } else {
            Vec::new()
        };

        // Like reqwest's text(): use the charset the server names when it is
        // known, and UTF-8 otherwise. Bytes that do not decode become U+FFFD.
        let encoding = charset
            .as_deref()
            .and_then(|charset| encoding_rs::Encoding::for_label(charset.as_bytes()))
            .unwrap_or(encoding_rs::UTF_8);
        let (decoded, _, _) = encoding.decode(&body);

        Ok(FetchedResponse {
            final_url: terminal.current.to_string(),
            redirects: terminal.redirects,
            status: terminal.status,
            headers,
            content_type,
            body: decoded.into_owned(),
            rendered: false,
        })
    }

    async fn build_bytes_response(
        &self,
        terminal: TerminalResponse,
    ) -> Result<FetchedBytesResponse, FetchError> {
        let ResponseMetadata {
            headers,
            content_type,
            ..
        } = response_metadata(&terminal.response);
        let bytes = self.read_capped_bytes(terminal.response).await?;

        Ok(FetchedBytesResponse {
            final_url: terminal.current.to_string(),
            redirects: terminal.redirects,
            status: terminal.status,
            headers,
            content_type,
            bytes,
        })
    }

    async fn read_capped_bytes(
        &self,
        mut response: reqwest::Response,
    ) -> Result<Vec<u8>, FetchError> {
        // When Content-Length is already too big, fail before reading.
        if let Some(content_length) = response.content_length() {
            if content_length > self.max_body_bytes as u64 {
                return Err(FetchError::BodyTooLarge {
                    limit: self.max_body_bytes,
                });
            }
        }

        // Read chunk by chunk and stop at the limit. `.bytes()` and `.text()`
        // would load the whole body first.
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(map_send_error)? {
            if chunk.len() > self.max_body_bytes.saturating_sub(body.len()) {
                return Err(FetchError::BodyTooLarge {
                    limit: self.max_body_bytes,
                });
            }
            body.extend_from_slice(&chunk);
        }

        Ok(body)
    }
}

#[async_trait]
impl Fetcher for HttpFetcher {
    async fn fetch(&self, url: &Url) -> Result<FetchedResponse, FetchError> {
        let header_origin = self.header_origin.as_ref().unwrap_or(url);
        let terminal = self
            .fetch_terminal_response(url, header_origin)
            .await
            .map_err(|failure| failure.error)?;
        self.build_response(terminal).await
    }
}

struct TerminalResponse {
    current: Url,
    redirects: Vec<RedirectHop>,
    status: u16,
    response: reqwest::Response,
}

struct ResponseMetadata {
    headers: ResponseHeaders,
    content_type: Option<String>,
    charset: Option<String>,
}

pub(crate) struct FetchedBytesFailure {
    pub(crate) error: FetchError,
    pub(crate) final_url: Option<String>,
    pub(crate) final_status: Option<u16>,
}

impl From<FetchError> for FetchedBytesFailure {
    fn from(error: FetchError) -> Self {
        Self {
            error,
            final_url: None,
            final_status: None,
        }
    }
}

fn observed_fetch_failure(url: &Url, status: u16, error: FetchError) -> FetchedBytesFailure {
    FetchedBytesFailure {
        error,
        final_url: Some(url.to_string()),
        final_status: Some(status),
    }
}

fn response_metadata(response: &reqwest::Response) -> ResponseMetadata {
    // Read the headers before the body. Reading the body consumes the
    // reqwest response, and the extraction still needs the headers.
    let mut headers = ResponseHeaders::new();
    for (name, value) in response.headers() {
        headers.push(
            name.as_str(),
            String::from_utf8_lossy(value.as_bytes()).into_owned(),
        );
    }

    // `content_type` is only the media type. The charset is used to decode
    // the body.
    let raw_content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let (content_type, charset) = match raw_content_type.as_deref() {
        Some(content_type) => parse_content_type(content_type),
        None => (None, None),
    };

    ResponseMetadata {
        headers,
        content_type,
        charset,
    }
}

fn map_send_error(error: reqwest::Error) -> FetchError {
    if error.is_timeout() {
        FetchError::Timeout
    } else {
        FetchError::Request {
            detail: describe_request_error(&error),
        }
    }
}

/// A readable cause for a failed request.
///
/// reqwest's own message only says "error sending request for url (...)".
/// The cause, such as a DNS failure or a refused connection, is in the
/// errors underneath it.
pub(crate) fn describe_request_error(error: &reqwest::Error) -> String {
    let mut causes = Vec::new();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        let text = cause.to_string();
        if !text.is_empty() && !causes.iter().any(|seen: &String| seen.contains(&text)) {
            causes.push(text);
        }
        source = cause.source();
    }
    let joined = causes.join(": ").to_ascii_lowercase();
    let (hint, advice) =
        if joined.contains("dns error") || joined.contains("failed to lookup address") {
            (Some("the host name could not be found in DNS"), None)
        } else if joined.contains("connection refused") || joined.contains("actively refused") {
            (
                Some("the connection was refused"),
                Some("Is the server running on that port?"),
            )
        } else if joined.contains("certificate") || joined.contains("tls") {
            (Some("the HTTPS connection could not be set up (TLS)"), None)
        } else if joined.contains("connection reset") {
            (Some("the server closed the connection"), None)
        } else {
            (None, None)
        };
    let text = match (hint, causes.last()) {
        (Some(hint), Some(cause)) => format!("{hint} ({cause})"),
        (Some(hint), None) => hint.to_owned(),
        (None, Some(_)) => causes.join(": "),
        (None, None) => error.to_string(),
    };
    match advice {
        Some(advice) => format!("{text}. {advice}"),
        None => text,
    }
}

/// Media types auditroo reads as text: HTML, XML, JSON and `text/*`.
pub(crate) fn is_text_media_type(media_type: &str) -> bool {
    media_type.starts_with("text/")
        || media_type.contains("html")
        || media_type.contains("xml")
        || media_type.contains("json")
}

/// Whether the user's custom headers may go to `target`.
///
/// Only the same host and port get them. A switch from HTTP to HTTPS keeps
/// them; a switch from HTTPS to HTTP drops them. `Url::port()` is `None` for
/// a default port, so `:443` and no port count as the same.
pub(crate) fn custom_header_target_matches_origin(original: &Url, target: &Url) -> bool {
    original.host_str().map(str::to_ascii_lowercase)
        == target.host_str().map(str::to_ascii_lowercase)
        && original.port() == target.port()
        && !(original.scheme() == "https" && target.scheme() == "http")
}

/// Split a Content-Type header into the media type and the charset.
///
/// The rest of auditroo only looks at the media type in lowercase, so a
/// small parser here is enough and no MIME crate is needed.
pub(crate) fn parse_content_type(raw: &str) -> (Option<String>, Option<String>) {
    let essence = raw.split(';').next().unwrap_or("").trim();
    let essence = essence.split_once('/').and_then(|(kind, subtype)| {
        (is_mime_token(kind) && is_mime_token(subtype) && !subtype.contains('/'))
            .then(|| essence.to_ascii_lowercase())
    });
    let charset = raw.split(';').skip(1).find_map(|parameter| {
        let (key, value) = parameter.split_once('=')?;
        key.trim()
            .eq_ignore_ascii_case("charset")
            .then(|| value.trim().trim_matches('"').to_owned())
    });

    (essence, charset)
}

fn is_mime_token(token: &str) -> bool {
    !token.is_empty()
        && token.bytes().all(|byte| {
            matches!(
                byte,
                b'A'..=b'Z'
                    | b'a'..=b'z'
                    | b'0'..=b'9'
                    | b'!'
                    | b'#'
                    | b'$'
                    | b'%'
                    | b'&'
                    | b'\''
                    | b'*'
                    | b'+'
                    | b'-'
                    | b'.'
                    | b'^'
                    | b'_'
                    | b'`'
                    | b'|'
                    | b'~'
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(input: &str) -> Url {
        Url::parse(input).expect("test URL parses")
    }

    #[test]
    fn header_target_matching_enforces_host_port_and_downgrade_policy() {
        assert!(custom_header_target_matches_origin(
            &url("http://example.com/a"),
            &url("https://example.com/b")
        ));
        assert!(!custom_header_target_matches_origin(
            &url("https://example.com/a"),
            &url("http://example.com/b")
        ));
        assert!(custom_header_target_matches_origin(
            &url("https://example.com/a"),
            &url("https://EXAMPLE.com/b")
        ));
        assert!(!custom_header_target_matches_origin(
            &url("https://example.com/a"),
            &url("https://www.example.com/b")
        ));
        assert!(!custom_header_target_matches_origin(
            &url("https://www.example.com/a"),
            &url("https://example.com/b")
        ));
        assert!(!custom_header_target_matches_origin(
            &url("https://example.com:8443/a"),
            &url("https://example.com:9443/b")
        ));
        assert!(custom_header_target_matches_origin(
            &url("https://example.com/a"),
            &url("https://example.com/b")
        ));
    }

    #[test]
    fn header_target_matching_can_reattach_only_on_an_allowed_return_target() {
        let original = url("https://example.com/a");
        assert!(!custom_header_target_matches_origin(
            &original,
            &url("https://other.example/b")
        ));
        assert!(custom_header_target_matches_origin(
            &original,
            &url("https://example.com/c")
        ));
    }

    #[test]
    fn config_debug_redacts_custom_header_values() {
        let config = HttpFetcherConfig {
            headers: vec![(
                "x-auditroo-bypass".to_owned(),
                "do-not-print-this-secret".to_owned(),
            )],
            ..HttpFetcherConfig::default()
        };

        let debug = format!("{config:?}");
        assert!(debug.contains("x-auditroo-bypass"));
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("do-not-print-this-secret"));
    }

    #[test]
    fn default_user_agent_contains_version_and_repository() {
        let user_agent = default_user_agent();
        assert!(user_agent.starts_with("auditroo/"));
        assert!(user_agent.contains(env!("CARGO_PKG_VERSION")));
        assert!(user_agent.contains(REPOSITORY_URL));
    }

    #[test]
    fn content_type_parser_returns_lowercase_essence_and_charset() {
        assert_eq!(
            parse_content_type("text/html; charset=utf-8"),
            (Some("text/html".to_owned()), Some("utf-8".to_owned()))
        );
        assert_eq!(
            parse_content_type("TEXT/HTML"),
            (Some("text/html".to_owned()), None)
        );
        assert_eq!(
            parse_content_type("application/json"),
            (Some("application/json".to_owned()), None)
        );
        assert_eq!(
            parse_content_type("application/xhtml+xml; Charset=\"UTF-8\""),
            (
                Some("application/xhtml+xml".to_owned()),
                Some("UTF-8".to_owned())
            )
        );
        assert_eq!(parse_content_type("garbage"), (None, None));
        assert_eq!(parse_content_type("text / html"), (None, None));
        assert_eq!(
            parse_content_type("text/html,application/json"),
            (None, None)
        );
    }

    #[test]
    fn header_line_helper_splits_on_first_colon_and_trims() {
        assert_eq!(
            HttpFetcherConfig::header_from_line("X-Test: one:two"),
            Some(("X-Test".to_owned(), "one:two".to_owned()))
        );
        assert_eq!(HttpFetcherConfig::header_from_line("missing"), None);
    }

    #[test]
    fn construction_rejects_custom_user_agent_headers() {
        for name in ["User-Agent", "user-agent"] {
            let config = HttpFetcherConfig {
                headers: vec![(name.to_owned(), "value".to_owned())],
                ..HttpFetcherConfig::default()
            };
            assert!(matches!(
                HttpFetcher::new(config),
                Err(FetchError::UserAgentHeaderNotAllowed { .. })
            ));
        }
    }

    #[test]
    fn construction_rejects_invalid_header_names_and_values() {
        let bad_name = HttpFetcherConfig {
            headers: vec![("Bad Header".to_owned(), "value".to_owned())],
            ..HttpFetcherConfig::default()
        };
        assert!(matches!(
            HttpFetcher::new(bad_name),
            Err(FetchError::InvalidHeader { .. })
        ));

        let bad_value = HttpFetcherConfig {
            headers: vec![("X-Test".to_owned(), "bad\nvalue".to_owned())],
            ..HttpFetcherConfig::default()
        };
        assert!(matches!(
            HttpFetcher::new(bad_value),
            Err(FetchError::InvalidHeader { .. })
        ));
    }

    #[test]
    fn construction_rejects_invalid_configured_user_agent() {
        let bad_user_agent = HttpFetcherConfig {
            user_agent: Some("bad\nagent".to_owned()),
            ..HttpFetcherConfig::default()
        };

        assert!(matches!(
            HttpFetcher::new(bad_user_agent),
            Err(FetchError::InvalidUserAgent { .. })
        ));
    }

    #[test]
    fn construction_accepts_default_config() {
        assert!(HttpFetcher::new(HttpFetcherConfig::default()).is_ok());
    }
}
