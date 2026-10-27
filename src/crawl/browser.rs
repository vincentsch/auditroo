use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

use async_trait::async_trait;
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::browser::BrowserContextId;
use chromiumoxide::cdp::browser_protocol::emulation::{
    SetLocaleOverrideParams, SetTimezoneOverrideParams,
};
use chromiumoxide::cdp::browser_protocol::fetch::{
    ContinueRequestParams, EventRequestPaused, FailRequestParams, HeaderEntry,
};
use chromiumoxide::cdp::browser_protocol::network::{
    ErrorReason, EventLoadingFailed, EventRequestWillBeSent, EventResponseReceived,
    EventResponseReceivedExtraInfo, Headers as CdpHeaders, ResourceType, Response as CdpResponse,
};
use chromiumoxide::cdp::browser_protocol::page::{
    EventLifecycleEvent, EventNavigatedWithinDocument, FrameId,
};
use chromiumoxide::cdp::browser_protocol::target::{
    CreateBrowserContextParams, CreateTargetParams,
};
use chromiumoxide::cdp::IntoEventKind;
use chromiumoxide::error::{CdpError, Result as CdpResult};
use chromiumoxide::handler::viewport::Viewport;
use chromiumoxide::listeners::EventStream;
use chromiumoxide::page::Page;
use futures_util::{FutureExt, StreamExt};
use reqwest::header::{HeaderName, HeaderValue};
use serde_json::Value;
use tempfile::TempDir;
use tokio::sync::{watch, Mutex};
use tokio::task::JoinHandle;
use tokio::time::{sleep, sleep_until, timeout, Instant};
use url::Url;

use super::fetch::{
    BackendUnavailableReason, FetchError, FetchedResponse, Fetcher, RedirectHop, ResponseHeaders,
};
use super::http::{custom_header_target_matches_origin, default_user_agent, parse_content_type};

const VIEWPORT_WIDTH: u32 = 1280;
const VIEWPORT_HEIGHT: u32 = 720;
const LAUNCH_TIMEOUT_MS: u64 = 30_000;
const CLEANUP_GRACE: Duration = Duration::from_secs(5);
const URL_STABLE_FOR: Duration = Duration::from_millis(500);
const URL_POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone)]
pub struct BrowserFetcherConfig {
    pub start_url: Url,
    pub user_agent: Option<String>,
    pub headers: Vec<(String, String)>,
    pub timeout_ms: u64,
    pub max_body_bytes: usize,
    pub chrome_executable: Option<PathBuf>,
}

impl fmt::Debug for BrowserFetcherConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let redacted_headers: Vec<_> = self
            .headers
            .iter()
            .map(|(name, _)| (name.as_str(), "[REDACTED]"))
            .collect();

        formatter
            .debug_struct("BrowserFetcherConfig")
            .field("start_url", &self.start_url)
            .field("user_agent", &self.user_agent)
            .field("headers", &redacted_headers)
            .field("timeout_ms", &self.timeout_ms)
            .field("max_body_bytes", &self.max_body_bytes)
            .field("chrome_executable", &self.chrome_executable)
            .finish()
    }
}

impl Default for BrowserFetcherConfig {
    fn default() -> Self {
        Self {
            start_url: Url::parse("http://localhost/").expect("default URL parses"),
            user_agent: None,
            headers: Vec::new(),
            timeout_ms: 10_000,
            max_body_bytes: 10_485_760,
            chrome_executable: None,
        }
    }
}

pub struct BrowserFetcher {
    inner: Arc<BrowserInner>,
    config: BrowserFetchRuntimeConfig,
}

#[derive(Clone)]
struct BrowserFetchRuntimeConfig {
    start_url: Url,
    user_agent: String,
    headers: Arc<Vec<BrowserHeader>>,
    timeout_ms: u64,
    max_body_bytes: usize,
}

#[derive(Clone)]
struct BrowserHeader {
    name: String,
    lower_name: String,
    value: String,
}

#[derive(Clone)]
pub struct BrowserSession {
    inner: Arc<BrowserInner>,
}

struct BrowserInner {
    browser: Mutex<Option<Browser>>,
    state_tx: watch::Sender<BackendState>,
    handler: Mutex<Option<JoinHandle<()>>>,
    shutdown_lock: Mutex<()>,
    _profile_dir: TempDir,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackendState {
    Running,
    Closing,
    Failed(BackendUnavailableReason),
    Closed,
}

impl BrowserFetcher {
    pub async fn launch(
        config: BrowserFetcherConfig,
    ) -> Result<(Self, BrowserSession), FetchError> {
        let user_agent = config.user_agent.unwrap_or_else(default_user_agent);
        HeaderValue::from_str(&user_agent).map_err(|error| FetchError::InvalidUserAgent {
            detail: error.to_string(),
        })?;

        let headers = validate_headers(config.headers)?;
        let profile_dir = tempfile::Builder::new()
            .prefix("auditroo-browser-")
            .tempdir()
            .map_err(|error| FetchError::ClientBuild {
                detail: format!("could not create a temporary Chrome profile folder: {error}"),
            })?;

        let mut builder = BrowserConfig::builder()
            .new_headless_mode()
            .disable_cache()
            .disable_https_first()
            .respect_https_errors()
            .surface_invalid_messages()
            .launch_timeout(Duration::from_millis(LAUNCH_TIMEOUT_MS))
            .request_timeout(Duration::from_millis(config.timeout_ms.max(1)))
            .viewport(Viewport {
                width: VIEWPORT_WIDTH,
                height: VIEWPORT_HEIGHT,
                device_scale_factor: Some(1.0),
                emulating_mobile: false,
                is_landscape: true,
                has_touch: false,
            })
            .window_size(VIEWPORT_WIDTH, VIEWPORT_HEIGHT)
            .user_data_dir(profile_dir.path());

        if !headers.is_empty() {
            builder = builder.enable_request_intercept();
        }
        if let Some(executable) = &config.chrome_executable {
            builder = builder.chrome_executable(executable);
        }

        let browser_config = builder.build().map_err(|error| FetchError::ClientBuild {
            detail: format!("could not set up Chrome: {error}"),
        })?;
        let (browser, handler) = Browser::launch(browser_config)
            .await
            .map_err(map_launch_error)?;

        let (state_tx, _state_rx) = watch::channel(BackendState::Running);
        let handler = tokio::spawn(run_handler(handler, state_tx.clone()));
        let inner = Arc::new(BrowserInner {
            browser: Mutex::new(Some(browser)),
            state_tx,
            handler: Mutex::new(Some(handler)),
            shutdown_lock: Mutex::new(()),
            _profile_dir: profile_dir,
        });
        let fetcher = BrowserFetcher {
            inner: Arc::clone(&inner),
            config: BrowserFetchRuntimeConfig {
                start_url: config.start_url,
                user_agent,
                headers: Arc::new(headers),
                timeout_ms: config.timeout_ms,
                max_body_bytes: config.max_body_bytes,
            },
        };
        let session = BrowserSession { inner };

        Ok((fetcher, session))
    }

    async fn fetch_rendered(&self, url: &Url) -> Result<FetchedResponse, FetchError> {
        ensure_backend_running(&self.inner)?;

        let deadline = Instant::now() + Duration::from_millis(self.config.timeout_ms.max(1));
        let context_id = {
            let browser = self.inner.browser.lock().await;
            let browser = browser.as_ref().ok_or_else(already_unavailable)?;
            self.with_deadline(
                deadline,
                browser.create_browser_context(
                    CreateBrowserContextParams::builder()
                        .dispose_on_detach(true)
                        .build(),
                ),
            )
            .await?
        };

        let page = match self.create_page(&context_id, deadline).await {
            Ok(page) => page,
            Err(error) => {
                self.cleanup_context(context_id).await;
                return Err(error);
            }
        };
        let page_for_cleanup = page.clone();
        let context_for_cleanup = context_id.clone();

        let result = self.render_page(url, page, deadline).await;
        self.cleanup_page_context(page_for_cleanup, context_for_cleanup)
            .await;
        result
    }

    async fn create_page(
        &self,
        context_id: &BrowserContextId,
        deadline: Instant,
    ) -> Result<Page, FetchError> {
        let params = CreateTargetParams::builder()
            .url("about:blank")
            .browser_context_id(context_id.clone())
            .width(VIEWPORT_WIDTH as i64)
            .height(VIEWPORT_HEIGHT as i64)
            .build()
            .map_err(|error| FetchError::ClientBuild {
                detail: format!("could not open a Chrome tab: {error}"),
            })?;
        let browser = self.inner.browser.lock().await;
        let browser = browser.as_ref().ok_or_else(already_unavailable)?;
        self.with_deadline(deadline, browser.new_page(params)).await
    }

    async fn render_page(
        &self,
        url: &Url,
        page: Page,
        deadline: Instant,
    ) -> Result<FetchedResponse, FetchError> {
        self.configure_page(&page, deadline).await?;

        let main_frame = self
            .with_deadline(deadline, page.mainframe())
            .await?
            .ok_or_else(|| request_error("Chrome did not report the page's main frame"))?;

        let page_tracker = Arc::new(Mutex::new(PageTracker::default()));
        let (page_error_tx, mut page_error_rx) = watch::channel(None);
        let collect_initial_redirects = Arc::new(AtomicBool::new(true));
        let mut listener_tasks = self
            .spawn_page_listeners(
                &page,
                &main_frame,
                Arc::clone(&page_tracker),
                Arc::clone(&collect_initial_redirects),
                page_error_tx.clone(),
                deadline,
            )
            .await?;

        let result = async {
            self.navigate(&page, url, deadline, &mut page_error_rx)
                .await?;
            collect_initial_redirects.store(false, Ordering::SeqCst);

            loop {
                let target = self
                    .wait_for_render_settle(
                        Arc::clone(&page_tracker),
                        &page,
                        deadline,
                        &mut page_error_rx,
                    )
                    .await?;
                let Some(metadata) = self
                    .wait_for_document_metadata(
                        Arc::clone(&page_tracker),
                        &target,
                        deadline,
                        &mut page_error_rx,
                    )
                    .await?
                else {
                    continue;
                };
                if matches!(metadata.status, 204 | 205) {
                    return Err(request_error("Chrome got no response for the page"));
                }

                let body = self
                    .with_page_deadline(deadline, page.content(), &mut page_error_rx)
                    .await?;
                if body.len() > self.config.max_body_bytes {
                    return Err(FetchError::BodyTooLarge {
                        limit: self.config.max_body_bytes,
                    });
                }
                let final_url = self
                    .current_http_url(&page, deadline, &mut page_error_rx)
                    .await?;
                let redirects = {
                    let tracker = page_tracker.lock().await;
                    if !tracker.capture_still_current(&target, &final_url) {
                        continue;
                    }
                    tracker.initial_redirects.clone()
                };

                return Ok(FetchedResponse {
                    final_url: final_url.to_string(),
                    redirects,
                    status: metadata.status,
                    headers: metadata.headers,
                    content_type: metadata.content_type,
                    body,
                    rendered: true,
                });
            }
        }
        .await;

        for task in listener_tasks.drain(..) {
            task.abort();
        }

        result
    }

    async fn navigate(
        &self,
        page: &Page,
        url: &Url,
        deadline: Instant,
        page_errors: &mut watch::Receiver<Option<FetchError>>,
    ) -> Result<(), FetchError> {
        self.with_page_deadline(deadline, page.goto(url.as_str()), page_errors)
            .await?;
        Ok(())
    }

    async fn configure_page(&self, page: &Page, deadline: Instant) -> Result<(), FetchError> {
        self.with_deadline(
            deadline,
            page.set_user_agent(self.config.user_agent.clone()),
        )
        .await?;
        self.with_deadline(
            deadline,
            page.emulate_locale(SetLocaleOverrideParams::builder().locale("en-US").build()),
        )
        .await?;
        self.with_deadline(
            deadline,
            page.emulate_timezone(SetTimezoneOverrideParams::new("UTC")),
        )
        .await?;
        Ok(())
    }

    async fn spawn_page_listeners(
        &self,
        page: &Page,
        main_frame: &FrameId,
        tracker: Arc<Mutex<PageTracker>>,
        collect_initial_redirects: Arc<AtomicBool>,
        page_error_tx: watch::Sender<Option<FetchError>>,
        deadline: Instant,
    ) -> Result<Vec<JoinHandle<()>>, FetchError> {
        let request_events = self
            .event_listener::<EventRequestWillBeSent>(page, deadline)
            .await?;
        let response_events = self
            .event_listener::<EventResponseReceived>(page, deadline)
            .await?;
        let extra_events = self
            .event_listener::<EventResponseReceivedExtraInfo>(page, deadline)
            .await?;
        let lifecycle_events = self
            .event_listener::<EventLifecycleEvent>(page, deadline)
            .await?;
        let same_document_events = self
            .event_listener::<EventNavigatedWithinDocument>(page, deadline)
            .await?;
        let loading_failed_events = self
            .event_listener::<EventLoadingFailed>(page, deadline)
            .await?;

        let mut tasks = vec![
            tokio::spawn(collect_request_events(
                request_events,
                main_frame.clone(),
                Arc::clone(&tracker),
                collect_initial_redirects,
            )),
            tokio::spawn(collect_response_events(
                response_events,
                main_frame.clone(),
                Arc::clone(&tracker),
            )),
            tokio::spawn(collect_extra_response_events(
                extra_events,
                Arc::clone(&tracker),
            )),
            tokio::spawn(collect_lifecycle_events(
                lifecycle_events,
                main_frame.clone(),
                Arc::clone(&tracker),
            )),
            tokio::spawn(collect_same_document_events(
                same_document_events,
                main_frame.clone(),
                Arc::clone(&tracker),
            )),
            tokio::spawn(collect_loading_failed_events(
                loading_failed_events,
                tracker,
            )),
        ];

        if !self.config.headers.is_empty() {
            let paused_events = self
                .event_listener::<EventRequestPaused>(page, deadline)
                .await?;
            tasks.push(tokio::spawn(continue_intercepted_requests(
                paused_events,
                page.clone(),
                self.config.start_url.clone(),
                Arc::clone(&self.config.headers),
                Arc::clone(&self.inner),
                page_error_tx,
            )));
        }

        Ok(tasks)
    }

    async fn event_listener<T>(
        &self,
        page: &Page,
        deadline: Instant,
    ) -> Result<EventStream<T>, FetchError>
    where
        T: IntoEventKind + Send + 'static,
    {
        self.with_deadline(deadline, page.event_listener::<T>())
            .await
    }

    async fn wait_for_render_settle(
        &self,
        tracker: Arc<Mutex<PageTracker>>,
        page: &Page,
        deadline: Instant,
        page_errors: &mut watch::Receiver<Option<FetchError>>,
    ) -> Result<CaptureTarget, FetchError> {
        let mut stable_target: Option<CaptureTarget> = None;
        let mut stable_since = Instant::now();

        loop {
            let final_url = self.current_http_url(page, deadline, page_errors).await?;
            let target = {
                let tracker = tracker.lock().await;
                tracker.capture_target(&final_url)?
            };

            match target {
                Some(target) => {
                    let unchanged = stable_target
                        .as_ref()
                        .is_some_and(|stable_target| stable_target == &target);
                    if unchanged
                        && Instant::now().saturating_duration_since(stable_since) >= URL_STABLE_FOR
                    {
                        return Ok(target);
                    }
                    if !unchanged {
                        stable_target = Some(target);
                        stable_since = Instant::now();
                    }
                }
                None => {
                    stable_target = None;
                    stable_since = Instant::now();
                }
            };

            self.wait_or_page_error(deadline, sleep(URL_POLL_INTERVAL), page_errors)
                .await?;
        }
    }

    async fn current_http_url(
        &self,
        page: &Page,
        deadline: Instant,
        page_errors: &mut watch::Receiver<Option<FetchError>>,
    ) -> Result<Url, FetchError> {
        let raw = self
            .with_page_deadline(deadline, page.url(), page_errors)
            .await?
            .ok_or_else(|| request_error("Chrome did not report the page's URL"))?;
        if raw.starts_with("chrome-error://") {
            return Err(request_error("Chrome showed its own error page"));
        }
        parse_http_url(&raw)
            .ok_or_else(|| request_error("the page ended on a URL that is not http:// or https://"))
    }

    async fn wait_for_document_metadata(
        &self,
        tracker: Arc<Mutex<PageTracker>>,
        target: &CaptureTarget,
        deadline: Instant,
        page_errors: &mut watch::Receiver<Option<FetchError>>,
    ) -> Result<Option<SelectedMetadata>, FetchError> {
        loop {
            let lookup = {
                let tracker = tracker.lock().await;
                if !tracker.capture_still_current(target, &target.final_url) {
                    return Ok(None);
                }
                tracker.select_metadata(target)
            };

            match lookup {
                MetadataLookup::Ready(metadata) => return Ok(Some(metadata)),
                MetadataLookup::None | MetadataLookup::PendingExtra => {}
            }

            self.wait_or_page_error(deadline, sleep(URL_POLL_INTERVAL), page_errors)
                .await?;
        }
    }

    async fn cleanup_page_context(&self, page: Page, context_id: BrowserContextId) {
        let _ = timeout(CLEANUP_GRACE, async {
            let _ = page.close().await;
            self.cleanup_context(context_id).await;
        })
        .await;
    }

    async fn cleanup_context(&self, context_id: BrowserContextId) {
        let browser = self.inner.browser.lock().await;
        if let Some(browser) = browser.as_ref() {
            let _ = browser.dispose_browser_context(context_id).await;
        }
    }

    async fn wait_or_timeout<F>(
        &self,
        deadline: Instant,
        future: F,
    ) -> Result<F::Output, FetchError>
    where
        F: Future,
    {
        let mut state_rx = self.inner.state_tx.subscribe();
        tokio::select! {
            output = future => Ok(output),
            _ = sleep_until(deadline) => Err(FetchError::Timeout),
            reason = wait_for_backend_failure(&mut state_rx) => {
                Err(FetchError::BackendUnavailable { reason })
            }
        }
    }

    async fn wait_or_page_error<F>(
        &self,
        deadline: Instant,
        future: F,
        page_errors: &mut watch::Receiver<Option<FetchError>>,
    ) -> Result<F::Output, FetchError>
    where
        F: Future,
    {
        let mut state_rx = self.inner.state_tx.subscribe();
        tokio::select! {
            output = future => Ok(output),
            _ = sleep_until(deadline) => Err(FetchError::Timeout),
            reason = wait_for_backend_failure(&mut state_rx) => {
                Err(FetchError::BackendUnavailable { reason })
            }
            error = wait_for_page_error(page_errors) => Err(error),
        }
    }

    async fn with_deadline<T, F>(&self, deadline: Instant, future: F) -> Result<T, FetchError>
    where
        F: Future<Output = CdpResult<T>>,
    {
        self.record_cdp_result(self.wait_or_timeout(deadline, future).await?)
    }

    async fn with_page_deadline<T, F>(
        &self,
        deadline: Instant,
        future: F,
        page_errors: &mut watch::Receiver<Option<FetchError>>,
    ) -> Result<T, FetchError>
    where
        F: Future<Output = CdpResult<T>>,
    {
        self.record_cdp_result(
            self.wait_or_page_error(deadline, future, page_errors)
                .await?,
        )
    }

    fn record_cdp_result<T>(&self, result: CdpResult<T>) -> Result<T, FetchError> {
        result.map_err(|error| {
            let error = map_cdp_error(error);
            if let FetchError::BackendUnavailable { reason } = error {
                mark_backend_failed(&self.inner, reason);
            }
            error
        })
    }
}

#[async_trait]
impl Fetcher for BrowserFetcher {
    async fn fetch(&self, url: &Url) -> Result<FetchedResponse, FetchError> {
        self.fetch_rendered(url).await
    }
}

impl BrowserSession {
    pub async fn shutdown(&self) -> Result<(), FetchError> {
        let _shutdown = self.inner.shutdown_lock.lock().await;
        let prior_state = *self.inner.state_tx.borrow();
        match prior_state {
            BackendState::Closed => return Ok(()),
            BackendState::Failed(reason) => {
                self.close_after_failure().await;
                return Err(FetchError::BackendUnavailable { reason });
            }
            BackendState::Closing => return Ok(()),
            BackendState::Running => {}
        }

        let _ = self.inner.state_tx.send(BackendState::Closing);
        let browser = {
            let mut browser = self.inner.browser.lock().await;
            browser.take()
        };
        let Some(browser) = browser else {
            let _ = self.inner.state_tx.send(BackendState::Closed);
            return Ok(());
        };

        let close_result = self.close_browser(browser).await;
        let join_result = self.join_handler().await;
        match close_result.and(join_result) {
            Ok(()) => {
                let _ = self.inner.state_tx.send(BackendState::Closed);
                Ok(())
            }
            Err(error) => {
                let _ = self.inner.state_tx.send(BackendState::Failed(
                    BackendUnavailableReason::ShutdownFailed,
                ));
                Err(error)
            }
        }
    }

    async fn close_after_failure(&self) {
        let mut browser = self.inner.browser.lock().await;
        if let Some(mut browser) = browser.take() {
            let _ = self.kill_and_wait(&mut browser).await;
        }
        drop(browser);
        let _ = self.join_handler().await;
        let _ = self.inner.state_tx.send(BackendState::Closed);
    }

    async fn close_browser(&self, mut browser: Browser) -> Result<(), FetchError> {
        match timeout(CLEANUP_GRACE, browser.close()).await {
            Ok(Ok(_)) => {
                if self.wait_for_browser_exit(&mut browser).await {
                    Ok(())
                } else {
                    self.kill_and_wait(&mut browser).await
                }
            }
            Ok(Err(_)) | Err(_) => self.kill_and_wait(&mut browser).await,
        }
    }

    async fn wait_for_browser_exit(&self, browser: &mut Browser) -> bool {
        matches!(timeout(CLEANUP_GRACE, browser.wait()).await, Ok(Ok(_)))
    }

    async fn kill_and_wait(&self, browser: &mut Browser) -> Result<(), FetchError> {
        let killed = timeout(CLEANUP_GRACE, browser.kill()).await;
        let exited = self.wait_for_browser_exit(browser).await;
        if killed.is_ok() && exited {
            Ok(())
        } else {
            Err(FetchError::BackendUnavailable {
                reason: BackendUnavailableReason::ShutdownFailed,
            })
        }
    }

    async fn join_handler(&self) -> Result<(), FetchError> {
        let mut handler = self.inner.handler.lock().await;
        let Some(mut handler) = handler.take() else {
            return Ok(());
        };
        match timeout(CLEANUP_GRACE, &mut handler).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(FetchError::BackendUnavailable {
                reason: BackendUnavailableReason::HandlerPanicked,
            }),
            Err(_) => {
                handler.abort();
                let _ = timeout(CLEANUP_GRACE, handler).await;
                Err(FetchError::BackendUnavailable {
                    reason: BackendUnavailableReason::ShutdownFailed,
                })
            }
        }
    }
}

async fn run_handler(
    mut handler: chromiumoxide::handler::Handler,
    state_tx: watch::Sender<BackendState>,
) {
    let reason = AssertUnwindSafe(async move {
        while let Some(result) = handler.next().await {
            if let Err(error) = result {
                if is_ignorable_protocol_compatibility_error(&error) {
                    continue;
                }
                return BackendUnavailableReason::HandlerFailed;
            }
        }
        BackendUnavailableReason::HandlerEnded
    })
    .catch_unwind()
    .await
    .unwrap_or(BackendUnavailableReason::HandlerPanicked);

    if *state_tx.borrow() == BackendState::Running {
        let _ = state_tx.send(BackendState::Failed(reason));
    }
}

fn ensure_backend_running(inner: &BrowserInner) -> Result<(), FetchError> {
    match *inner.state_tx.borrow() {
        BackendState::Running => Ok(()),
        BackendState::Failed(reason) => Err(FetchError::BackendUnavailable { reason }),
        BackendState::Closing | BackendState::Closed => Err(already_unavailable()),
    }
}

fn mark_backend_failed(inner: &BrowserInner, reason: BackendUnavailableReason) {
    if *inner.state_tx.borrow() == BackendState::Running {
        let _ = inner.state_tx.send(BackendState::Failed(reason));
    }
}

async fn wait_for_backend_failure(
    state_rx: &mut watch::Receiver<BackendState>,
) -> BackendUnavailableReason {
    loop {
        match *state_rx.borrow() {
            BackendState::Running => {}
            BackendState::Closing => return BackendUnavailableReason::AlreadyUnavailable,
            BackendState::Failed(reason) => return reason,
            BackendState::Closed => return BackendUnavailableReason::AlreadyUnavailable,
        }
        if state_rx.changed().await.is_err() {
            return BackendUnavailableReason::AlreadyUnavailable;
        }
    }
}

async fn wait_for_page_error(page_errors: &mut watch::Receiver<Option<FetchError>>) -> FetchError {
    loop {
        if let Some(error) = page_errors.borrow().clone() {
            return error;
        }
        if page_errors.changed().await.is_err() {
            return request_error("the Chrome tab closed before the page loaded");
        }
    }
}

fn already_unavailable() -> FetchError {
    FetchError::BackendUnavailable {
        reason: BackendUnavailableReason::AlreadyUnavailable,
    }
}

fn validate_headers(headers: Vec<(String, String)>) -> Result<Vec<BrowserHeader>, FetchError> {
    let mut output = Vec::with_capacity(headers.len());
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("user-agent") {
            return Err(FetchError::UserAgentHeaderNotAllowed { name });
        }
        let header_name =
            HeaderName::from_bytes(name.as_bytes()).map_err(|error| FetchError::InvalidHeader {
                name: name.clone(),
                detail: error.to_string(),
            })?;
        HeaderValue::from_str(&value).map_err(|error| FetchError::InvalidHeader {
            name: name.clone(),
            detail: error.to_string(),
        })?;
        output.push(BrowserHeader {
            name: header_name.to_string(),
            lower_name: header_name.as_str().to_ascii_lowercase(),
            value,
        });
    }
    Ok(output)
}

fn map_launch_error(error: CdpError) -> FetchError {
    match error {
        CdpError::LaunchExit(_, _) => FetchError::BackendUnavailable {
            reason: BackendUnavailableReason::ProcessExited,
        },
        CdpError::LaunchTimeout(_) | CdpError::Timeout => FetchError::Timeout,
        CdpError::LaunchIo(_, _) | CdpError::Io(_) | CdpError::Ws(_) | CdpError::NoResponse => {
            FetchError::BackendUnavailable {
                reason: BackendUnavailableReason::CdpDisconnected,
            }
        }
        other => FetchError::Request {
            detail: safe_cdp_detail(&other),
        },
    }
}

fn is_ignorable_protocol_compatibility_error(error: &CdpError) -> bool {
    matches!(
        error,
        CdpError::InvalidMessage(message, _)
            if message.contains("\"method\":\"Network.requestWillBeSentExtraInfo\"")
    )
}

fn map_cdp_error(error: CdpError) -> FetchError {
    match error {
        CdpError::Timeout => FetchError::Timeout,
        CdpError::ChannelSendError(_) | CdpError::NoResponse | CdpError::Ws(_) => {
            FetchError::BackendUnavailable {
                reason: BackendUnavailableReason::CdpDisconnected,
            }
        }
        CdpError::LaunchExit(_, _) => FetchError::BackendUnavailable {
            reason: BackendUnavailableReason::ProcessExited,
        },
        other => FetchError::Request {
            detail: safe_cdp_detail(&other),
        },
    }
}

fn safe_cdp_detail(error: &CdpError) -> String {
    match error {
        CdpError::ChromeMessage(message) if message.contains("net::ERR_ABORTED") => {
            "Chrome stopped loading the page (net::ERR_ABORTED)".to_owned()
        }
        CdpError::ChromeMessage(_) => "Chrome refused a command".to_owned(),
        CdpError::FrameNotFound(_) => "Chrome lost the page's frame".to_owned(),
        CdpError::NotFound => "Chrome did not return a value auditroo asked for".to_owned(),
        CdpError::JavascriptException(_) => {
            "a script threw an error while auditroo read the page".to_owned()
        }
        CdpError::InvalidMessage(_, _) => {
            "Chrome sent a message auditroo could not read".to_owned()
        }
        _ => "Chrome refused a command".to_owned(),
    }
}

fn request_error(detail: &str) -> FetchError {
    FetchError::Request {
        detail: detail.to_owned(),
    }
}

async fn collect_request_events(
    mut events: EventStream<EventRequestWillBeSent>,
    main_frame: FrameId,
    tracker: Arc<Mutex<PageTracker>>,
    collect_initial_redirects: Arc<AtomicBool>,
) {
    while let Some(event) = events.next().await {
        tracker.lock().await.push_request(
            (*event).clone(),
            &main_frame,
            collect_initial_redirects.load(Ordering::SeqCst),
        );
    }
}

async fn collect_response_events(
    mut events: EventStream<EventResponseReceived>,
    main_frame: FrameId,
    tracker: Arc<Mutex<PageTracker>>,
) {
    while let Some(event) = events.next().await {
        tracker
            .lock()
            .await
            .push_response((*event).clone(), &main_frame);
    }
}

async fn collect_extra_response_events(
    mut events: EventStream<EventResponseReceivedExtraInfo>,
    tracker: Arc<Mutex<PageTracker>>,
) {
    while let Some(event) = events.next().await {
        tracker.lock().await.push_extra((*event).clone());
    }
}

async fn collect_lifecycle_events(
    mut events: EventStream<EventLifecycleEvent>,
    main_frame: FrameId,
    tracker: Arc<Mutex<PageTracker>>,
) {
    while let Some(event) = events.next().await {
        tracker
            .lock()
            .await
            .push_lifecycle((*event).clone(), &main_frame);
    }
}

async fn collect_same_document_events(
    mut events: EventStream<EventNavigatedWithinDocument>,
    main_frame: FrameId,
    tracker: Arc<Mutex<PageTracker>>,
) {
    while let Some(event) = events.next().await {
        tracker
            .lock()
            .await
            .push_same_document((*event).clone(), &main_frame);
    }
}

async fn collect_loading_failed_events(
    mut events: EventStream<EventLoadingFailed>,
    tracker: Arc<Mutex<PageTracker>>,
) {
    while let Some(event) = events.next().await {
        tracker.lock().await.push_loading_failed((*event).clone());
    }
}

async fn continue_intercepted_requests(
    mut events: EventStream<EventRequestPaused>,
    page: Page,
    start_url: Url,
    headers: Arc<Vec<BrowserHeader>>,
    inner: Arc<BrowserInner>,
    page_error_tx: watch::Sender<Option<FetchError>>,
) {
    let mut failing_page = false;
    while let Some(event) = events.next().await {
        if failing_page || page_error_tx.borrow().is_some() {
            if let Err(error) = fail_intercepted_request(&page, &event).await {
                record_interception_error(&inner, &page_error_tx, error);
            }
            failing_page = true;
            continue;
        }

        let params = intercepted_request_params(&event, &start_url, &headers);
        if let Err(error) = page.execute(params).await {
            record_interception_error(&inner, &page_error_tx, error);
            let _ = fail_intercepted_request(&page, &event).await;
            failing_page = true;
        }
    }
}

async fn fail_intercepted_request(page: &Page, event: &EventRequestPaused) -> CdpResult<()> {
    page.execute(FailRequestParams::new(
        event.request_id.clone(),
        ErrorReason::Aborted,
    ))
    .await
    .map(|_| ())
}

fn record_interception_error(
    inner: &BrowserInner,
    page_error_tx: &watch::Sender<Option<FetchError>>,
    error: CdpError,
) {
    let error = map_cdp_error(error);
    if let FetchError::BackendUnavailable { reason } = error {
        mark_backend_failed(inner, reason);
    }
    let _ = page_error_tx.send_if_modified(|slot| {
        if slot.is_none() {
            *slot = Some(error.clone());
            true
        } else {
            false
        }
    });
}

fn intercepted_request_params(
    event: &EventRequestPaused,
    start_url: &Url,
    headers: &[BrowserHeader],
) -> ContinueRequestParams {
    let target = Url::parse(&event.request.url).ok();
    let should_attach = target
        .as_ref()
        .is_some_and(|target| custom_header_target_matches_origin(start_url, target));
    let stripped_names = headers
        .iter()
        .map(|header| header.lower_name.as_str())
        .collect::<BTreeSet<_>>();
    let mut outgoing = Vec::new();

    for (name, value) in header_entries(event.request.headers.inner()) {
        if !stripped_names.contains(name.to_ascii_lowercase().as_str()) {
            outgoing.push(HeaderEntry::new(name, value));
        }
    }
    if should_attach {
        outgoing.extend(
            headers
                .iter()
                .map(|header| HeaderEntry::new(header.name.clone(), header.value.clone())),
        );
    }

    ContinueRequestParams::builder()
        .request_id(event.request_id.clone())
        .headers(outgoing)
        .build()
        .expect("request id is set")
}

#[derive(Default)]
struct PageTracker {
    active_loader: Option<String>,
    active_loaded: bool,
    active_failed: bool,
    generation: u64,
    request_loaders: BTreeMap<String, String>,
    initial_redirects: Vec<RedirectHop>,
    responses: ResponseTracker,
}

impl PageTracker {
    fn push_request(
        &mut self,
        event: EventRequestWillBeSent,
        main_frame: &FrameId,
        collect_initial_redirects: bool,
    ) {
        if event.frame_id.as_ref() != Some(main_frame)
            || event.r#type.as_ref() != Some(&ResourceType::Document)
        {
            return;
        }

        let request_id = event.request_id.inner().clone();
        let loader_id = event.loader_id.inner().clone();
        if let Some(response) = event.redirect_response {
            self.responses.push_redirect(
                request_id.clone(),
                loader_id.clone(),
                response.clone(),
                event.redirect_has_extra_info,
            );
            if collect_initial_redirects {
                self.push_initial_redirect(&response, &event.request.url);
            }
        }

        self.request_loaders.insert(request_id, loader_id.clone());
        if self.active_loader.as_deref() != Some(loader_id.as_str()) {
            self.active_loader = Some(loader_id);
            self.active_loaded = false;
            self.active_failed = false;
            self.generation = self.generation.saturating_add(1);
        }
    }

    fn push_initial_redirect(&mut self, response: &CdpResponse, to: &str) {
        let status = response.status as u16;
        if !is_redirect_status(status) {
            return;
        }
        let Some(from) = parse_http_url(&response.url) else {
            return;
        };
        let Some(to) = parse_http_url(to) else {
            return;
        };
        self.initial_redirects.push(RedirectHop {
            from: from.to_string(),
            status,
            to: to.to_string(),
        });
    }

    fn push_response(&mut self, event: EventResponseReceived, main_frame: &FrameId) {
        if event.r#type != ResourceType::Document
            || event
                .frame_id
                .as_ref()
                .is_some_and(|frame_id| frame_id != main_frame)
        {
            return;
        }
        self.responses.push_response(event, main_frame);
    }

    fn push_extra(&mut self, event: EventResponseReceivedExtraInfo) {
        self.responses.push_extra(event);
    }

    fn push_lifecycle(&mut self, event: EventLifecycleEvent, main_frame: &FrameId) {
        if event.frame_id != *main_frame || event.name != "load" {
            return;
        }
        if self.active_loader.as_deref() == Some(event.loader_id.as_ref()) {
            self.active_loaded = true;
            self.generation = self.generation.saturating_add(1);
        }
    }

    fn push_same_document(&mut self, event: EventNavigatedWithinDocument, main_frame: &FrameId) {
        if event.frame_id != *main_frame {
            return;
        }
        if parse_http_url(&event.url).is_some() {
            self.generation = self.generation.saturating_add(1);
        }
    }

    fn push_loading_failed(&mut self, event: EventLoadingFailed) {
        if event.r#type != ResourceType::Document {
            return;
        }
        let request_id = event.request_id.inner();
        if self
            .request_loaders
            .get(request_id)
            .is_some_and(|loader| self.active_loader.as_ref() == Some(loader))
        {
            self.active_failed = true;
            self.generation = self.generation.saturating_add(1);
        }
    }

    fn capture_target(&self, final_url: &Url) -> Result<Option<CaptureTarget>, FetchError> {
        if self.active_failed {
            return Err(request_error("Chrome could not load the page"));
        }
        let Some(loader_id) = &self.active_loader else {
            return Ok(None);
        };
        if !self.active_loaded {
            return Ok(None);
        }

        Ok(Some(CaptureTarget {
            loader_id: loader_id.clone(),
            generation: self.generation,
            final_url: final_url.clone(),
        }))
    }

    fn capture_still_current(&self, target: &CaptureTarget, final_url: &Url) -> bool {
        !self.active_failed
            && self.active_loaded
            && self.active_loader.as_deref() == Some(target.loader_id.as_str())
            && self.generation == target.generation
            && final_url == &target.final_url
    }

    fn select_metadata(&self, target: &CaptureTarget) -> MetadataLookup {
        self.responses.select(target)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CaptureTarget {
    loader_id: String,
    generation: u64,
    final_url: Url,
}

#[derive(Default)]
struct ResponseTracker {
    slots: Vec<DocumentResponseSlot>,
    pending_extra: BTreeMap<String, VecDeque<ResponseExtra>>,
}

impl ResponseTracker {
    fn push_redirect(
        &mut self,
        request_id: String,
        loader_id: String,
        response: CdpResponse,
        has_extra_info: bool,
    ) {
        self.push_slot(request_id, loader_id, response, has_extra_info);
    }

    fn push_response(&mut self, event: EventResponseReceived, main_frame: &FrameId) {
        if event.r#type != ResourceType::Document
            || event
                .frame_id
                .as_ref()
                .is_some_and(|frame_id| frame_id != main_frame)
        {
            return;
        }

        let request_id = event.request_id.inner().clone();
        let loader_id = event.loader_id.inner().clone();
        self.push_slot(request_id, loader_id, event.response, event.has_extra_info);
    }

    fn push_slot(
        &mut self,
        request_id: String,
        loader_id: String,
        response: CdpResponse,
        has_extra_info: bool,
    ) {
        let extra = self
            .pending_extra
            .get_mut(&request_id)
            .and_then(VecDeque::pop_front);
        let slot = DocumentResponseSlot {
            request_id,
            loader_id,
            url: parse_http_url(&response.url),
            status: response.status as u16,
            headers: headers_from_cdp(&response.headers),
            has_extra_info,
            extra,
        };
        self.slots.push(slot);
    }

    fn push_extra(&mut self, event: EventResponseReceivedExtraInfo) {
        let request_id = event.request_id.inner().clone();
        let extra = ResponseExtra {
            status: event.status_code as u16,
            headers: headers_from_cdp(&event.headers),
        };
        if let Some(slot) = self.slots.iter_mut().find(|slot| {
            slot.request_id == request_id && slot.requires_extra_info() && slot.extra.is_none()
        }) {
            slot.extra = Some(extra);
        } else {
            self.pending_extra
                .entry(request_id)
                .or_default()
                .push_back(extra);
        }
    }

    fn select(&self, target: &CaptureTarget) -> MetadataLookup {
        let latest_exact_non_redirect = self.slots.iter().rev().find(|slot| {
            slot.loader_id == target.loader_id
                && slot.url.as_ref() == Some(&target.final_url)
                && !is_redirect_status(slot.status)
        });
        let latest_non_redirect =
            self.slots.iter().rev().find(|slot| {
                slot.loader_id == target.loader_id && !is_redirect_status(slot.status)
            });
        let latest_exact = self.slots.iter().rev().find(|slot| {
            slot.loader_id == target.loader_id && slot.url.as_ref() == Some(&target.final_url)
        });
        let latest_any = self
            .slots
            .iter()
            .rev()
            .find(|slot| slot.loader_id == target.loader_id);
        let Some(slot) = latest_exact_non_redirect
            .or(latest_non_redirect)
            .or(latest_exact)
            .or(latest_any)
        else {
            return MetadataLookup::None;
        };

        if slot.requires_extra_info() && slot.extra.is_none() {
            return MetadataLookup::PendingExtra;
        }

        let (status, headers) = match &slot.extra {
            Some(extra) => (extra.status, extra.headers.clone()),
            None => (slot.status, slot.headers.clone()),
        };
        MetadataLookup::Ready(SelectedMetadata {
            status,
            content_type: content_type_from_headers(&headers),
            headers,
        })
    }
}

struct DocumentResponseSlot {
    request_id: String,
    loader_id: String,
    url: Option<Url>,
    status: u16,
    headers: ResponseHeaders,
    has_extra_info: bool,
    extra: Option<ResponseExtra>,
}

impl DocumentResponseSlot {
    fn requires_extra_info(&self) -> bool {
        self.has_extra_info
    }
}

#[derive(Clone)]
struct ResponseExtra {
    status: u16,
    headers: ResponseHeaders,
}

enum MetadataLookup {
    Ready(SelectedMetadata),
    PendingExtra,
    None,
}

struct SelectedMetadata {
    status: u16,
    headers: ResponseHeaders,
    content_type: Option<String>,
}

fn is_redirect_status(status: u16) -> bool {
    (300..400).contains(&status)
}

fn parse_http_url(raw: &str) -> Option<Url> {
    let url = Url::parse(raw).ok()?;
    matches!(url.scheme(), "http" | "https").then_some(url)
}

fn content_type_from_headers(headers: &ResponseHeaders) -> Option<String> {
    headers
        .get_all_case_insensitive("content-type")
        .iter()
        .find_map(|value| parse_content_type(value).0)
}

fn headers_from_cdp(headers: &CdpHeaders) -> ResponseHeaders {
    let mut output = ResponseHeaders::new();
    for (name, value) in header_entries(headers.inner()) {
        for value in value.split('\n') {
            output.push(&name, value.to_owned());
        }
    }
    output
}

fn header_entries(value: &Value) -> Vec<(String, String)> {
    let Some(object) = value.as_object() else {
        return Vec::new();
    };

    object
        .iter()
        .map(|(name, value)| {
            let value = value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| value.to_string());
            (name.clone(), value)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chromiumoxide::cdp::browser_protocol::fetch::RequestId;
    use chromiumoxide::cdp::browser_protocol::network::{
        EventResponseReceived, EventResponseReceivedExtraInfo, Headers, Request,
        RequestReferrerPolicy, ResourcePriority,
    };

    fn url(input: &str) -> Url {
        Url::parse(input).expect("test URL parses")
    }

    #[test]
    fn http_url_parser_preserves_fragments_for_browser_capture() {
        let parsed =
            parse_http_url("HTTP://EXAMPLE.test:443/page?x=1#ready").expect("HTTP URL parses");

        assert_eq!(parsed.to_string(), "http://example.test:443/page?x=1#ready");
    }

    fn paused_request(target: &str, headers: Value) -> EventRequestPaused {
        EventRequestPaused {
            request_id: RequestId::from("intercept-1".to_owned()),
            request: Request::builder()
                .url(target)
                .method("GET")
                .headers(Headers::new(headers))
                .initial_priority(ResourcePriority::Medium)
                .referrer_policy(RequestReferrerPolicy::NoReferrer)
                .build()
                .expect("request builds"),
            frame_id: FrameId::from("frame-1".to_owned()),
            resource_type: ResourceType::Document,
            response_error_reason: None,
            response_status_code: None,
            response_status_text: None,
            response_headers: None,
            network_id: None,
            redirected_request_id: None,
        }
    }

    fn cdp_response(target: &str, status: u16, headers: Value, mime_type: &str) -> CdpResponse {
        serde_json::from_value(serde_json::json!({
            "url": target,
            "status": status,
            "statusText": "",
            "headers": headers,
            "mimeType": mime_type,
            "charset": "",
            "connectionReused": false,
            "connectionId": 0.0,
            "encodedDataLength": 0.0,
            "securityState": "unknown"
        }))
        .expect("CDP response fixture parses")
    }

    fn response_event(
        request_id: &str,
        loader_id: &str,
        frame_id: &FrameId,
        target: &str,
        status: u16,
        headers: Value,
        has_extra_info: bool,
    ) -> EventResponseReceived {
        serde_json::from_value(serde_json::json!({
            "requestId": request_id,
            "loaderId": loader_id,
            "timestamp": 1.0,
            "type": "Document",
            "response": {
                "url": target,
                "status": status,
                "statusText": "",
                "headers": headers,
                "mimeType": "text/html",
                "charset": "",
                "connectionReused": false,
                "connectionId": 0.0,
                "encodedDataLength": 0.0,
                "securityState": "unknown"
            },
            "hasExtraInfo": has_extra_info,
            "frameId": frame_id.as_ref()
        }))
        .expect("response event fixture parses")
    }

    fn extra_event(
        request_id: &str,
        status: u16,
        headers: Value,
    ) -> EventResponseReceivedExtraInfo {
        serde_json::from_value(serde_json::json!({
            "requestId": request_id,
            "blockedCookies": [],
            "headers": headers,
            "resourceIPAddressSpace": "Unknown",
            "statusCode": status
        }))
        .expect("extra-info fixture parses")
    }

    fn target(loader_id: &str, target: &str) -> CaptureTarget {
        CaptureTarget {
            loader_id: loader_id.to_owned(),
            generation: 1,
            final_url: url(target),
        }
    }

    fn ready_metadata(lookup: MetadataLookup) -> SelectedMetadata {
        match lookup {
            MetadataLookup::Ready(metadata) => metadata,
            MetadataLookup::PendingExtra => panic!("metadata is still pending extra-info"),
            MetadataLookup::None => panic!("metadata was not selected"),
        }
    }

    #[test]
    fn config_debug_redacts_header_values() {
        let config = BrowserFetcherConfig {
            headers: vec![("x-owner-token".to_owned(), "secret".to_owned())],
            ..BrowserFetcherConfig::default()
        };

        let debug = format!("{config:?}");
        assert!(debug.contains("x-owner-token"));
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("secret"));
    }

    #[test]
    fn intercepted_request_only_attaches_custom_headers_to_allowed_origin() {
        let headers = validate_headers(vec![("x-owner-token".to_owned(), "secret".to_owned())])
            .expect("headers validate");
        let event = paused_request(
            "https://example.test/page",
            serde_json::json!({
                "Accept": "text/html",
                "X-Owner-Token": "stale",
            }),
        );

        let params = intercepted_request_params(&event, &url("https://example.test/"), &headers);
        let emitted = params.headers.expect("headers are overridden");
        assert_eq!(
            emitted
                .iter()
                .filter(|header| header.name.eq_ignore_ascii_case("x-owner-token"))
                .map(|header| header.value.as_str())
                .collect::<Vec<_>>(),
            ["secret"]
        );
        assert!(emitted
            .iter()
            .any(|header| header.name == "Accept" && header.value == "text/html"));
    }

    #[test]
    fn intercepted_request_strips_custom_headers_after_origin_change() {
        let headers = validate_headers(vec![("x-owner-token".to_owned(), "secret".to_owned())])
            .expect("headers validate");
        let event = paused_request(
            "https://other.test/page",
            serde_json::json!({
                "Accept": "text/html",
                "X-Owner-Token": "stale",
            }),
        );

        let params = intercepted_request_params(&event, &url("https://example.test/"), &headers);
        let emitted = params.headers.expect("headers are overridden");
        assert!(emitted
            .iter()
            .all(|header| !header.name.eq_ignore_ascii_case("x-owner-token")));
        assert!(emitted
            .iter()
            .any(|header| header.name == "Accept" && header.value == "text/html"));
    }

    #[test]
    fn cdp_headers_split_newline_joined_repeated_values() {
        let headers = headers_from_cdp(&CdpHeaders::new(serde_json::json!({
            "Set-Cookie": "a=1\nb=2",
            "Content-Type": "text/html; charset=utf-8",
        })));

        assert_eq!(
            headers.get_all_case_insensitive("set-cookie"),
            ["a=1", "b=2"]
        );
        assert_eq!(
            headers.get_all_case_insensitive("content-type"),
            ["text/html; charset=utf-8"]
        );
    }

    #[test]
    fn handler_ignores_only_request_extra_info_decode_compatibility_error() {
        let ignored = CdpError::InvalidMessage(
            r#"{"method":"Network.requestWillBeSentExtraInfo","params":{"headers":{"X-Secret":"value"}}}"#
                .to_owned(),
            serde_json::from_str::<Value>("not json").expect_err("fixture is invalid JSON"),
        );
        let other = CdpError::InvalidMessage(
            r#"{"method":"Network.responseReceivedExtraInfo","params":{}}"#.to_owned(),
            serde_json::from_str::<Value>("not json").expect_err("fixture is invalid JSON"),
        );

        assert!(is_ignorable_protocol_compatibility_error(&ignored));
        assert!(!is_ignorable_protocol_compatibility_error(&other));
    }

    #[test]
    fn response_slots_pair_redirect_and_terminal_extra_info_in_order() {
        let frame = FrameId::from("frame-1".to_owned());
        let mut tracker = ResponseTracker::default();

        tracker.push_redirect(
            "request-1".to_owned(),
            "loader-1".to_owned(),
            cdp_response(
                "https://example.test/redirect",
                302,
                serde_json::json!({"Content-Type": "text/plain"}),
                "text/plain",
            ),
            true,
        );
        tracker.push_extra(extra_event(
            "request-1",
            302,
            serde_json::json!({"Content-Type": "text/plain"}),
        ));
        tracker.push_response(
            response_event(
                "request-1",
                "loader-1",
                &frame,
                "https://example.test/final",
                200,
                serde_json::json!({"Content-Type": "application/json"}),
                true,
            ),
            &frame,
        );
        assert!(matches!(
            tracker.select(&target("loader-1", "https://example.test/final")),
            MetadataLookup::PendingExtra
        ));

        tracker.push_extra(extra_event(
            "request-1",
            201,
            serde_json::json!({
                "Content-Type": "text/html; charset=utf-8",
                "X-Robots-Tag": "noindex\nnofollow"
            }),
        ));

        let metadata =
            ready_metadata(tracker.select(&target("loader-1", "https://example.test/final")));
        assert_eq!(metadata.status, 201);
        assert_eq!(metadata.content_type.as_deref(), Some("text/html"));
        assert_eq!(
            metadata.headers.get_all_case_insensitive("x-robots-tag"),
            ["noindex", "nofollow"]
        );
    }

    #[test]
    fn response_selection_is_limited_to_the_active_loader() {
        let frame = FrameId::from("frame-1".to_owned());
        let mut tracker = ResponseTracker::default();
        tracker.push_response(
            response_event(
                "request-1",
                "loader-old",
                &frame,
                "https://example.test/old",
                200,
                serde_json::json!({"Content-Type": "text/html"}),
                false,
            ),
            &frame,
        );
        tracker.push_response(
            response_event(
                "request-2",
                "loader-new",
                &frame,
                "https://example.test/new",
                404,
                serde_json::json!({"Content-Type": "text/plain"}),
                false,
            ),
            &frame,
        );

        let metadata =
            ready_metadata(tracker.select(&target("loader-new", "https://example.test/new")));
        assert_eq!(metadata.status, 404);
        assert_eq!(metadata.content_type.as_deref(), Some("text/plain"));
        assert!(matches!(
            tracker.select(&target("loader-missing", "https://example.test/new")),
            MetadataLookup::None
        ));
    }
}
