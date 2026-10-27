//! The internal link graph and orphan pages.

use std::collections::{BTreeMap, BTreeSet};

use url::Url;

use crate::crawl::{normalize_url, CrawlItem};
use crate::extract::{contributes_outbound_edges, SeoRecord};
use crate::site::Site;

/// The internal links of each crawled URL. Targets are in `normalize_url`
/// form.
pub type LinkGraph = BTreeMap<String, Vec<String>>;

/// Collects the internal links of each page while the crawl runs.
#[derive(Debug, Clone, Default)]
pub struct LinkGraphAccumulator {
    adjacency: BTreeMap<String, BTreeSet<String>>,
}

impl LinkGraphAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the links of a fetched page. When the URL redirected, the
    /// links belong to the document it ended on, so they are recorded under
    /// the final URL.
    pub fn observe(&mut self, item: &CrawlItem, record: &SeoRecord) {
        self.adjacency.entry(record.url.clone()).or_default();
        if !contributes_outbound_edges(record) {
            return;
        }
        let source = if record.navigation_changed() {
            match Url::parse(&record.final_url) {
                Ok(url) => normalize_url(&url),
                Err(_) => return,
            }
        } else {
            record.url.clone()
        };
        self.adjacency
            .entry(source)
            .or_default()
            .extend(item.discovered_links.iter().cloned());
    }

    pub fn finish(self) -> LinkGraph {
        self.adjacency
            .into_iter()
            .map(|(url, neighbors)| (url, neighbors.into_iter().collect()))
            .collect()
    }
}

pub fn build_link_graph<'a>(
    observations: impl IntoIterator<Item = (&'a CrawlItem, &'a SeoRecord)>,
) -> LinkGraph {
    let mut accumulator = LinkGraphAccumulator::new();
    for (item, record) in observations {
        accumulator.observe(item, record);
    }
    accumulator.finish()
}

/// Where a link leads. A link to a URL that redirects counts as a link to
/// the redirect's final URL.
pub fn resolve_link_target(target: &str, records: &BTreeMap<String, SeoRecord>) -> String {
    match records.get(target) {
        Some(record) if record.navigation_changed() => Url::parse(&record.final_url)
            .map(|url| normalize_url(&url))
            .unwrap_or_else(|_| record.final_url.clone()),
        _ => target.to_owned(),
    }
}

/// Pages that link to each URL, with links through redirects resolved.
pub fn inbound_links(
    graph: &LinkGraph,
    records: &BTreeMap<String, SeoRecord>,
) -> BTreeMap<String, BTreeSet<String>> {
    let mut inbound: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (source, targets) in graph {
        for target in targets {
            let resolved = resolve_link_target(target, records);
            if &resolved != source {
                inbound.entry(resolved).or_default().insert(source.clone());
            }
            if target != source {
                inbound
                    .entry(target.clone())
                    .or_default()
                    .insert(source.clone());
            }
        }
    }
    inbound
}

/// Pages a visitor can reach by following links from the start page.
/// Links through redirects count as links to the redirect's final URL.
pub fn reachable(
    records: &BTreeMap<String, SeoRecord>,
    graph: &LinkGraph,
    start_url: &str,
) -> BTreeSet<String> {
    let mut reached = BTreeSet::new();
    let mut queue = vec![start_url.to_owned()];
    while let Some(url) = queue.pop() {
        if !reached.insert(url.clone()) {
            continue;
        }
        let resolved = resolve_link_target(&url, records);
        if resolved != url {
            queue.push(resolved);
        }
        if let Some(targets) = graph.get(&url) {
            queue.extend(
                targets
                    .iter()
                    .filter(|target| !reached.contains(*target))
                    .cloned(),
            );
        }
    }
    reached
}

/// Indexable pages that cannot be reached by following links from the start
/// page: pages nothing links to, and groups of pages that only link to each
/// other.
pub fn orphans(
    records: &BTreeMap<String, SeoRecord>,
    graph: &LinkGraph,
    start_url: &str,
    site: &Site,
) -> Vec<String> {
    let reached = reachable(records, graph, start_url);
    records
        .values()
        .filter(|record| site.is_indexable(record))
        .map(|record| record.url.clone())
        .filter(|url| !reached.contains(url))
        .collect()
}
