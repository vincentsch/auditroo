//! A small documentation site for regression tests, modeled on a typical
//! static docs build: 24 pages, a sitemap index, hreflang between an English
//! and a German page, JSON-LD on some pages and a noindex 404 page.
//!
//! Absolute URLs in the pages use `{{ORIGIN}}`. Serving the same site with a
//! different origin there is how a preview build that still carries
//! production URLs is simulated.

use std::collections::BTreeMap;
use std::time::Duration;

use super::{FixtureResponse, FixtureSite};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Route {
    pub fn html(body: String) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type".into(), "text/html; charset=utf-8".into())],
            body,
        }
    }

    pub fn status(status: u16) -> Self {
        Self {
            status,
            headers: vec![("content-type".into(), "text/html".into())],
            body: String::new(),
        }
    }

    pub fn redirect(status: u16, location: &str) -> Self {
        Self {
            status,
            headers: vec![("location".into(), location.into())],
            body: String::new(),
        }
    }
}

/// Path to response. Unknown paths get `not_found` with status 404.
#[derive(Debug, Clone)]
pub struct DocsSite {
    pub routes: BTreeMap<String, Route>,
    pub not_found: String,
}

pub const REFERENCE_PAGES: [&str; 7] = [
    "capture", "verify", "demo", "export", "import", "inspect", "sign",
];

struct PageSpec {
    path: String,
    title: String,
    h1: String,
    description: String,
    lang: &'static str,
    json_ld: Option<String>,
    hreflang: bool,
    links: Vec<String>,
}

pub fn not_found_page() -> String {
    "<!doctype html><html><head><title>Not found | Lantern</title><meta name=\"robots\" content=\"noindex\"></head><body><h1>Page not found</h1></body></html>".to_owned()
}

impl DocsSite {
    pub fn new() -> Self {
        let docs = [
            "quickstart",
            "faq",
            "verify",
            "capture",
            "commands",
            "package-format",
            "timestamps",
            "for-recipients",
            "reference",
        ];
        let mut pages = vec![PageSpec {
            path: "/".into(),
            title: "Lantern: sealed evidence packages".into(),
            h1: "Lantern".into(),
            description: "Lantern seals files into evidence packages.".into(),
            lang: "en",
            json_ld: Some(r#"{"@context":"https://schema.org","@type":"SoftwareApplication","name":"Lantern"}"#.into()),
            hreflang: false,
            links: vec!["/docs/".into(), "/privacy/".into()],
        }];
        pages.push(PageSpec {
            path: "/docs/".into(),
            title: "Documentation | Lantern".into(),
            h1: "Documentation".into(),
            description: "All Lantern guides.".into(),
            lang: "en",
            json_ld: None,
            hreflang: false,
            links: docs.iter().map(|doc| format!("/docs/{doc}/")).collect(),
        });
        for doc in docs {
            let mut links = vec!["/docs/".to_owned()];
            if doc == "reference" {
                links.extend(
                    REFERENCE_PAGES
                        .iter()
                        .map(|page| format!("/docs/reference/lantern_{page}/")),
                );
            }
            if doc == "quickstart" {
                links.push("/docs/timestamps/".into());
                links.push("/images/diagram.png".into());
            }
            if doc == "for-recipients" {
                links.push("/docs/de/fuer-empfaenger/".into());
            }
            pages.push(PageSpec {
                path: format!("/docs/{doc}/"),
                title: format!("{} | Lantern", title_case(doc)),
                h1: title_case(doc),
                description: format!("How {} works in Lantern.", doc.replace('-', " ")),
                lang: "en",
                json_ld: matches!(doc, "capture" | "verify").then(|| {
                    format!(
                        r#"{{"@context":"https://schema.org","@type":"TechArticle","headline":"{}"}}"#,
                        title_case(doc)
                    )
                }),
                hreflang: doc == "for-recipients",
                links,
            });
        }
        for page in REFERENCE_PAGES {
            pages.push(PageSpec {
                path: format!("/docs/reference/lantern_{page}/"),
                title: format!("lantern {page} | Lantern"),
                h1: format!("lantern {page}"),
                description: format!("Reference for lantern {page}."),
                lang: "en",
                json_ld: None,
                hreflang: false,
                links: vec!["/docs/reference/".into()],
            });
        }
        pages.push(PageSpec {
            path: "/docs/de/fuer-empfaenger/".into(),
            title: "Für Empfänger | Lantern".into(),
            h1: "Für Empfänger".into(),
            description: "So prüfst du ein Paket.".into(),
            lang: "de",
            json_ld: None,
            hreflang: true,
            links: vec!["/docs/de/abnahme/".into(), "/docs/for-recipients/".into()],
        });
        pages.push(PageSpec {
            path: "/docs/de/abnahme/".into(),
            title: "Abnahme | Lantern".into(),
            h1: "Abnahme".into(),
            description: "Pakete abnehmen.".into(),
            lang: "de",
            json_ld: None,
            hreflang: false,
            links: vec!["/docs/de/fuer-empfaenger/".into()],
        });
        pages.push(PageSpec {
            path: "/privacy/".into(),
            title: "Privacy | Lantern".into(),
            h1: "Privacy".into(),
            description: "What Lantern stores.".into(),
            lang: "en",
            json_ld: None,
            hreflang: false,
            links: Vec::new(),
        });

        let mut routes = BTreeMap::new();
        let mut sitemap = String::from(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n",
        );
        for page in &pages {
            sitemap.push_str(&format!(
                "<url><loc>{{{{ORIGIN}}}}{}</loc></url>\n",
                page.path
            ));
            routes.insert(page.path.clone(), Route::html(render(page)));
        }
        sitemap.push_str("</urlset>\n");
        routes.insert(
            "/sitemap-0.xml".into(),
            Route {
                status: 200,
                headers: vec![("content-type".into(), "application/xml".into())],
                body: sitemap,
            },
        );
        routes.insert(
            "/sitemap-index.xml".into(),
            Route {
                status: 200,
                headers: vec![("content-type".into(), "application/xml".into())],
                body: "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<sitemapindex xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\"><sitemap><loc>{{ORIGIN}}/sitemap-0.xml</loc></sitemap></sitemapindex>\n".into(),
            },
        );
        routes.insert(
            "/robots.txt".into(),
            Route {
                status: 200,
                headers: vec![("content-type".into(), "text/plain".into())],
                body: "User-agent: *\nAllow: /\n\nSitemap: {{ORIGIN}}/sitemap-index.xml\n".into(),
            },
        );
        routes.insert(
            "/images/diagram.png".into(),
            Route {
                status: 200,
                headers: vec![("content-type".into(), "image/png".into())],
                body: "\u{89}PNG fake image bytes".into(),
            },
        );
        Self {
            routes,
            not_found: not_found_page(),
        }
    }

    /// Change one page's HTML. Panics when nothing changed, so a typo in a
    /// test does not pass silently.
    pub fn edit(&mut self, path: &str, edit: impl Fn(&str) -> String) {
        let route = self
            .routes
            .get_mut(path)
            .unwrap_or_else(|| panic!("no route {path}"));
        let changed = edit(&route.body);
        assert_ne!(changed, route.body, "edit of {path} changed nothing");
        route.body = changed;
    }

    /// Change every HTML page; panics when no page changed.
    pub fn edit_pages(&mut self, edit: impl Fn(&str) -> String) {
        let mut changed_any = false;
        for route in self.routes.values_mut() {
            if !route
                .headers
                .iter()
                .any(|(_, value)| value.starts_with("text/html"))
            {
                continue;
            }
            let changed = edit(&route.body);
            changed_any |= changed != route.body;
            route.body = changed;
        }
        assert!(changed_any, "edit_pages changed nothing");
    }

    pub fn set(&mut self, path: &str, route: Route) {
        self.routes.insert(path.to_owned(), route);
    }

    pub fn header(&mut self, path: &str, name: &str, value: &str) {
        self.routes
            .get_mut(path)
            .unwrap_or_else(|| panic!("no route {path}"))
            .headers
            .push((name.to_owned(), value.to_owned()));
    }

    pub fn remove(&mut self, path: &str) {
        self.routes
            .remove(path)
            .unwrap_or_else(|| panic!("no route {path}"));
    }

    /// Serve the site. `{{ORIGIN}}` becomes `origin`, or the server's own
    /// origin when `origin` is `None`.
    pub async fn serve(&self, origin: Option<&str>) -> FixtureSite {
        let server = FixtureSite::spawn().await;
        self.replace_on(&server, origin).await;
        server
    }

    /// Serve this site from `server` instead of what it served before, on
    /// the same address. That is how a deployment to the same host looks.
    pub async fn replace_on(&self, server: &FixtureSite, origin: Option<&str>) {
        let own = server.url("/").to_string();
        let own = own.trim_end_matches('/').to_owned();
        let origin = origin.unwrap_or(&own).to_owned();
        server.routes.lock().await.clear();
        for (path, route) in &self.routes {
            server
                .set_response(
                    path,
                    FixtureResponse {
                        status: route.status,
                        headers: route.headers.clone(),
                        body: route.body.replace("{{ORIGIN}}", &origin).into_bytes(),
                        delay: Duration::ZERO,
                    },
                )
                .await;
        }
        server
            .set_not_found(self.not_found.replace("{{ORIGIN}}", &origin))
            .await;
    }
}

fn render(page: &PageSpec) -> String {
    let mut head = format!(
        "<meta charset=\"utf-8\">\n<title>{}</title>\n<meta name=\"description\" content=\"{}\">\n<link rel=\"canonical\" href=\"{{{{ORIGIN}}}}{}\">\n",
        page.title, page.description, page.path
    );
    if page.hreflang {
        head.push_str("<link rel=\"alternate\" hreflang=\"en\" href=\"{{ORIGIN}}/docs/for-recipients/\"><link rel=\"alternate\" hreflang=\"de\" href=\"{{ORIGIN}}/docs/de/fuer-empfaenger/\">\n");
    }
    if let Some(json_ld) = &page.json_ld {
        head.push_str(&format!(
            "<script type=\"application/ld+json\">{json_ld}</script>\n"
        ));
    }
    let links: String = page
        .links
        .iter()
        .map(|link| format!("<li><a href=\"{link}\">{link}</a></li>"))
        .collect();
    format!(
        "<!doctype html>\n<html lang=\"{}\">\n<head>\n{head}</head>\n<body>\n<nav><a href=\"/\">Home</a> <a href=\"/docs/\">Docs</a></nav>\n<main>\n<h1>{}</h1>\n<h2>Overview</h2>\n<ul>{links}</ul>\n</main>\n</body>\n</html>\n",
        page.lang, page.h1
    )
}

fn title_case(slug: &str) -> String {
    let words: Vec<String> = slug
        .split('-')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect();
    words.join(" ")
}
