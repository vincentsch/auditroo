//! `auditroo demo`: a small coffee shop site and a preview of it with typical
//! deploy mistakes, both served from this process on 127.0.0.1.
//!
//! The same sites back the integration tests, so the demo output stays true.

use std::net::SocketAddr;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use url::Url;

use crate::audit::{crawl, evaluate, CrawlExtras, Evaluation};
use crate::cli::OutputArgs;
use crate::config::ResolvedSettings;
use crate::error::{CliError, EXIT_OK};
use crate::progress::Progress;
use crate::report::ReportCommand;

/// Which copy of the demo site to serve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    /// The site as it is live.
    Production,
    /// The same site after a change with several SEO mistakes.
    Preview,
}

struct Page {
    path: &'static str,
    title: &'static str,
    description: &'static str,
    h1: &'static str,
    lang: &'static str,
    body: &'static str,
    json_ld: Option<&'static str>,
}

const PAGES: &[Page] = &[
    Page {
        path: "/",
        title: "Harbor Coffee Roasters",
        description: "Small-batch coffee roasted every week in Portland.",
        h1: "Fresh coffee, roasted every week",
        lang: "en",
        body: r#"<p>Pick a coffee from <a href="/coffee/">our current list</a>.</p>"#,
        json_ld: Some(
            r#"{"@context":"https://schema.org","@type":"Organization","name":"Harbor Coffee Roasters"}"#,
        ),
    },
    Page {
        path: "/coffee/",
        title: "Our coffee | Harbor Coffee Roasters",
        description: "All coffees we roast right now.",
        h1: "Our coffee",
        lang: "en",
        body: r#"<ul><li><a href="/coffee/ethiopia-yirgacheffe/">Ethiopia Yirgacheffe</a></li><li><a href="/coffee/colombia-huila/">Colombia Huila</a></li><li><a href="/coffee/house-blend/">House Blend</a></li></ul>"#,
        json_ld: None,
    },
    Page {
        path: "/coffee/ethiopia-yirgacheffe/",
        title: "Ethiopia Yirgacheffe | Harbor Coffee Roasters",
        description: "Floral and bright, with notes of lemon and jasmine.",
        h1: "Ethiopia Yirgacheffe",
        lang: "en",
        body: "<p>Floral and bright.</p>",
        json_ld: Some(
            r#"{"@context":"https://schema.org","@type":"Product","name":"Ethiopia Yirgacheffe","offers":{"@type":"Offer","price":"16.00","priceCurrency":"USD"}}"#,
        ),
    },
    Page {
        path: "/coffee/colombia-huila/",
        title: "Colombia Huila | Harbor Coffee Roasters",
        description: "Sweet and round, with caramel and red apple.",
        h1: "Colombia Huila",
        lang: "en",
        body: "<p>Sweet and round.</p>",
        json_ld: Some(
            r#"{"@context":"https://schema.org","@type":"Product","name":"Colombia Huila","offers":{"@type":"Offer","price":"15.00","priceCurrency":"USD"}}"#,
        ),
    },
    Page {
        path: "/coffee/house-blend/",
        title: "House Blend | Harbor Coffee Roasters",
        description: "Our everyday coffee for espresso and filter.",
        h1: "House Blend",
        lang: "en",
        body: "<p>Chocolate and nuts.</p>",
        json_ld: Some(
            r#"{"@context":"https://schema.org","@type":"Product","name":"House Blend","offers":{"@type":"Offer","price":"13.00","priceCurrency":"USD"}}"#,
        ),
    },
    Page {
        path: "/brewing-guide/",
        title: "How to brew filter coffee | Harbor Coffee Roasters",
        description: "A step-by-step guide to better filter coffee at home.",
        h1: "How to brew filter coffee",
        lang: "en",
        body: "<p>Use 60 grams of coffee for one litre of water.</p>",
        json_ld: None,
    },
    Page {
        path: "/about/",
        title: "About us | Harbor Coffee Roasters",
        description: "Who roasts your coffee.",
        h1: "About us",
        lang: "en",
        body: "<p>We started roasting in a garage in 2019.</p>",
        json_ld: None,
    },
    Page {
        path: "/contact/",
        title: "Contact | Harbor Coffee Roasters",
        description: "How to reach Harbor Coffee Roasters.",
        h1: "Contact",
        lang: "en",
        body: "<p>Write to hello@harbor-coffee.test.</p>",
        json_ld: None,
    },
    Page {
        path: "/de/",
        title: "Harbor Coffee Roasters auf Deutsch",
        description: "Frisch gerösteter Kaffee aus Portland.",
        h1: "Frischer Kaffee, jede Woche geröstet",
        lang: "de",
        body: r#"<p>Unsere Kaffees findest du <a href="/coffee/">hier</a>.</p>"#,
        json_ld: None,
    },
];

/// A demo site on 127.0.0.1 with an OS-assigned port. It stops when dropped.
pub struct DemoServer {
    addr: SocketAddr,
    task: JoinHandle<()>,
}

impl DemoServer {
    pub async fn start(variant: Variant) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let origin = format!("http://{addr}");
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let origin = origin.clone();
                tokio::spawn(async move {
                    let _ = serve_connection(stream, variant, &origin).await;
                });
            }
        });
        Ok(Self { addr, task })
    }

    pub fn url(&self) -> String {
        format!("http://{}/", self.addr)
    }
}

impl Drop for DemoServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve_connection(
    mut stream: TcpStream,
    variant: Variant,
    origin: &str,
) -> std::io::Result<()> {
    let mut request = Vec::new();
    let mut buffer = [0u8; 2048];
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = stream.read(&mut buffer).await?;
        if read == 0 || request.len() > 16 * 1024 {
            return Ok(());
        }
        request.extend_from_slice(&buffer[..read]);
    }
    let head = String::from_utf8_lossy(&request);
    let path = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_owned();
    let (status, content_type, body) = respond(variant, origin, &path);
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Unknown",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

/// Status, content type and body for one request path.
fn respond(variant: Variant, origin: &str, path: &str) -> (u16, &'static str, String) {
    let preview = variant == Variant::Preview;
    match path {
        "/robots.txt" => {
            let mut body = "User-agent: *\n".to_owned();
            if preview {
                // Mistake: the new rule hides the brewing guide from Google.
                body.push_str("Disallow: /brewing-guide/\n");
            }
            body.push_str(&format!("Allow: /\n\nSitemap: {origin}/sitemap.xml\n"));
            (200, "text/plain; charset=utf-8", body)
        }
        "/sitemap.xml" => {
            let mut body = String::from(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n",
            );
            for page in PAGES {
                // Mistake: the contact page fell out of the sitemap.
                if preview && page.path == "/contact/" {
                    continue;
                }
                body.push_str(&format!("  <url><loc>{origin}{}</loc></url>\n", page.path));
            }
            body.push_str("</urlset>\n");
            (200, "application/xml", body)
        }
        // Mistake: the about page now crashes.
        "/about/" if preview => (
            500,
            "text/html; charset=utf-8",
            "<!doctype html><html><head><title>Server error</title></head><body><h1>Something went wrong</h1></body></html>".to_owned(),
        ),
        _ => match PAGES.iter().find(|page| page.path == path) {
            Some(page) => (200, "text/html; charset=utf-8", render_page(page, variant, origin)),
            None => (
                404,
                "text/html; charset=utf-8",
                "<!doctype html><html><head><title>Page not found</title><meta name=\"robots\" content=\"noindex\"></head><body><h1>Page not found</h1></body></html>".to_owned(),
            ),
        },
    }
}

fn render_page(page: &Page, variant: Variant, origin: &str) -> String {
    let preview = variant == Variant::Preview;
    let mut head = String::new();
    let mut title = page.title.to_owned();
    let mut canonical = format!("{origin}{}", page.path);
    let mut json_ld = page.json_ld.map(str::to_owned);
    if preview {
        match page.path {
            // Mistake: a template flag left noindex on a product page.
            "/coffee/ethiopia-yirgacheffe/" => {
                head.push_str("<meta name=\"robots\" content=\"noindex\">\n");
            }
            // Mistake: the canonical tag uses the staging host.
            "/coffee/colombia-huila/" => {
                canonical = format!("https://staging.harbor-coffee.test{}", page.path);
            }
            // Mistake: a trailing comma breaks the JSON-LD.
            "/coffee/house-blend/" => {
                json_ld = json_ld.map(|value| value.replacen('}', ",}", 1));
            }
            // A change that is fine but worth a look: a new title.
            "/contact/" => title = "Contact us | Harbor Coffee Roasters".to_owned(),
            _ => {}
        }
    }
    if matches!(page.path, "/" | "/de/") {
        head.push_str(&format!(
            "<link rel=\"alternate\" hreflang=\"en\" href=\"{origin}/\">\n<link rel=\"alternate\" hreflang=\"de\" href=\"{origin}/de/\">\n"
        ));
    }
    if let Some(json_ld) = json_ld {
        head.push_str(&format!(
            "<script type=\"application/ld+json\">{json_ld}</script>\n"
        ));
    }
    // Mistake: the new navigation links to a page that does not exist yet.
    let nav_extra = if preview {
        " <a href=\"/shipping/\">Shipping</a>"
    } else {
        ""
    };
    format!(
        "<!doctype html>\n<html lang=\"{lang}\">\n<head>\n<meta charset=\"utf-8\">\n<title>{title}</title>\n<meta name=\"description\" content=\"{description}\">\n<link rel=\"canonical\" href=\"{canonical}\">\n{head}</head>\n<body>\n<nav><a href=\"/\">Home</a> <a href=\"/coffee/\">Coffee</a> <a href=\"/brewing-guide/\">Brewing guide</a> <a href=\"/about/\">About</a> <a href=\"/contact/\">Contact</a> <a href=\"/de/\">Deutsch</a>{nav_extra}</nav>\n<main>\n<h1>{h1}</h1>\n{body}\n</main>\n</body>\n</html>\n",
        lang = page.lang,
        description = page.description,
        h1 = page.h1,
        body = page.body,
    )
}

/// Crawl settings for the demo: built-in defaults without auditroo.toml, no
/// rate limit, and robots.txt ignored for crawling. robots.txt is still
/// checked for Googlebot.
pub fn demo_settings() -> ResolvedSettings {
    let mut settings = crate::config::default_settings();
    settings.crawl.rate_limit_ms = 0;
    settings.crawl.respect_robots = false;
    settings
}

pub(crate) async fn run(output: OutputArgs) -> Result<i32, CliError> {
    let production = DemoServer::start(Variant::Production)
        .await
        .map_err(|error| CliError::runtime(format!("could not start the demo server: {error}")))?;
    let preview = DemoServer::start(Variant::Preview)
        .await
        .map_err(|error| CliError::runtime(format!("could not start the demo server: {error}")))?;
    let human = output.is_human();
    if human {
        println!(
            "auditroo demo compares a small coffee shop site with a preview of it that has typical deploy mistakes.\nBoth sites run on this computer. Production is at {} and the preview is at {}.\n",
            production.url(),
            preview.url()
        );
    }

    let settings = demo_settings();
    let mut progress = Progress::quiet();
    let started = std::time::Instant::now();
    let production_url = Url::parse(&production.url()).expect("demo URL parses");
    let preview_url = Url::parse(&preview.url()).expect("demo URL parses");
    let baseline = crawl(
        production_url,
        &settings,
        CrawlExtras::default(),
        &mut progress,
        "Crawling production",
    )
    .await?;
    let extras = CrawlExtras::compared_with(&baseline, &preview_url);
    let current = crawl(
        preview_url,
        &settings,
        extras,
        &mut progress,
        "Crawling preview",
    )
    .await?;
    let report = evaluate(Evaluation {
        command: ReportCommand::Compare,
        baseline: Some(&baseline),
        current: &current,
        settings: &settings,
        notes: Vec::new(),
    })
    .await?;
    output.emit(&report, Some(started.elapsed()))?;
    if human {
        println!(
            "\nIn CI, auditroo would exit with code {} here and fail the build. To try it on your own site:\n  auditroo check https://your-site.example\n  auditroo compare https://your-site.example https://your-preview.example",
            report.exit_code()
        );
    }
    Ok(EXIT_OK)
}
