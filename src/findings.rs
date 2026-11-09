//! Findings: what `check`, `diff` and `compare` report.
//!
//! Every finding has a rule from the list below. The rule sets the default
//! severity and the line that sums up a group of findings ("3 pages now have
//! noindex"). The message of a finding is one plain sentence about one URL.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::external::ImpactAnnotation;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Low,
    Medium,
    High,
    Critical,
}

impl Severity {
    pub const ALL_DESCENDING: [Severity; 5] = [
        Severity::Critical,
        Severity::High,
        Severity::Medium,
        Severity::Low,
        Severity::Info,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::Info => "Info",
            Self::Low => "Low",
            Self::Medium => "Medium",
            Self::High => "High",
            Self::Critical => "Critical",
        }
    }
}

impl fmt::Display for Severity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for Severity {
    type Err = String;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        match input.to_ascii_lowercase().as_str() {
            "info" => Ok(Self::Info),
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "critical" => Ok(Self::Critical),
            _ => Err(format!(
                "unknown severity {input:?}; use critical, high, medium, low or info"
            )),
        }
    }
}

macro_rules! rules {
    ($( $variant:ident => $id:literal, $severity:ident, $one:literal, $many:literal; )*) => {
        /// Every kind of finding auditroo reports.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum Rule {
            $( $variant, )*
        }

        impl Rule {
            pub const ALL: &'static [Rule] = &[ $( Rule::$variant, )* ];

            /// Stable id, used in JSON, ignore rules and the docs.
            pub fn id(self) -> &'static str {
                match self { $( Rule::$variant => $id, )* }
            }

            /// The severity a finding gets unless the code that raises it
            /// picks another one.
            pub fn severity(self) -> Severity {
                match self { $( Rule::$variant => Severity::$severity, )* }
            }

            /// Summary phrase for `count` findings. `{n}` is the count and
            /// `{group}` the shared detail, such as a host name.
            fn phrase(self, count: usize) -> &'static str {
                match self { $( Rule::$variant => if count == 1 { $one } else { $many }, )* }
            }
        }
    };
}

rules! {
    // Changes between a baseline and the current crawl.
    NoindexAdded => "noindex_added", Critical,
        "{n} page now has noindex", "{n} pages now have noindex";
    NoindexRemoved => "noindex_removed", Medium,
        "{n} page lost its noindex and can now be indexed",
        "{n} pages lost their noindex and can now be indexed";
    PageBroken => "page_broken", Critical,
        "{n} page now returns an error", "{n} pages now return an error";
    PageRedirected => "page_redirected", Medium,
        "{n} page now redirects{group}", "{n} pages now redirect{group}";
    FileBroken => "file_broken", High,
        "{n} file now returns an error", "{n} files now return an error";
    PageNotHtml => "page_not_html", Critical,
        "{n} page is no longer an HTML page", "{n} pages are no longer HTML pages";
    RedirectLoop => "redirect_loop", High,
        "{n} URL is stuck in a redirect loop", "{n} URLs are stuck in a redirect loop";
    RedirectChanged => "redirect_changed", High,
        "{n} redirect changed", "{n} redirects changed";
    FetchFailed => "fetch_failed", Medium,
        "{n} page could not be loaded", "{n} pages could not be loaded";
    CanonicalExternal => "canonical_external", High,
        "{n} canonical tag points to another host{group}",
        "{n} canonical tags point to another host{group}";
    CanonicalChanged => "canonical_changed", High,
        "{n} canonical tag changed", "{n} canonical tags changed";
    CanonicalRemoved => "canonical_removed", Medium,
        "{n} canonical tag was removed", "{n} canonical tags were removed";
    TitleChanged => "title_changed", Medium,
        "{n} title changed{group}", "{n} titles changed{group}";
    TitleRemoved => "title_removed", Medium,
        "{n} page lost its title", "{n} pages lost their title";
    DescriptionChanged => "description_changed", Low,
        "{n} meta description changed", "{n} meta descriptions changed";
    DescriptionRemoved => "description_removed", Low,
        "{n} page lost its meta description", "{n} pages lost their meta description";
    H1Changed => "h1_changed", Low,
        "{n} H1 heading changed", "{n} H1 headings changed";
    H1Removed => "h1_removed", Medium,
        "{n} page lost its H1 heading", "{n} pages lost their H1 heading";
    HeadingsChanged => "headings_changed", Info,
        "{n} page has different H2 or H3 headings", "{n} pages have different H2 or H3 headings";
    StructuredDataRemoved => "structured_data_removed", Medium,
        "{n} page lost its structured data", "{n} pages lost their structured data";
    StructuredDataChanged => "structured_data_changed", Low,
        "{n} page has different structured data types", "{n} pages have different structured data types";
    HreflangChanged => "hreflang_changed", Medium,
        "{n} page has different hreflang links", "{n} pages have different hreflang links";
    Orphaned => "orphaned", High,
        "{n} page can no longer be reached through links",
        "{n} pages can no longer be reached through links";
    SitemapUrlsRemoved => "sitemap_urls_removed", High,
        "{n} URL was removed from the sitemap", "{n} URLs were removed from the sitemap";
    SitemapBroken => "sitemap_broken", High,
        "{n} sitemap file is broken", "{n} sitemap files are broken";
    RobotsBlocked => "robots_blocked", High,
        "robots.txt now blocks {group} from {n} page", "robots.txt now blocks {group} from {n} pages";
    RobotsError => "robots_error", Critical,
        "robots.txt cannot be read", "robots.txt cannot be read";
    PageAdded => "page_added", Info,
        "{n} new page", "{n} new pages";
    PageFixed => "page_fixed", Info,
        "{n} page works again", "{n} pages work again";

    // Problems in one crawl. `diff` reports the ones that are new.
    BrokenLink => "broken_link", High,
        "{n} linked URL is broken", "{n} linked URLs are broken";
    RedirectChain => "redirect_chain", Low,
        "{n} linked URL redirects more than once", "{n} linked URLs redirect more than once";
    CanonicalToRedirect => "canonical_to_redirect", Medium,
        "{n} canonical tag points to a redirect", "{n} canonical tags point to a redirect";
    CanonicalToError => "canonical_to_error", High,
        "{n} canonical tag points to a broken page", "{n} canonical tags point to a broken page";
    CanonicalToNoindex => "canonical_to_noindex", Medium,
        "{n} canonical tag points to a noindex page", "{n} canonical tags point to a noindex page";
    SitemapUrlError => "sitemap_url_error", High,
        "the sitemap lists {n} URL that returns an error", "the sitemap lists {n} URLs that return an error";
    SitemapUrlRedirect => "sitemap_url_redirect", Low,
        "the sitemap lists {n} URL that redirects{group}", "the sitemap lists {n} URLs that redirect{group}";
    SitemapUrlNoindex => "sitemap_url_noindex", Medium,
        "the sitemap lists {n} noindex page", "the sitemap lists {n} noindex pages";
    SitemapUrlBlocked => "sitemap_url_blocked", High,
        "the sitemap lists {n} URL that robots.txt blocks for Googlebot",
        "the sitemap lists {n} URLs that robots.txt blocks for Googlebot";
    SitemapUrlExternal => "sitemap_url_external", High,
        "the sitemap lists {n} URL on another host{group}",
        "the sitemap lists {n} URLs on another host{group}";
    SitemapUrlCanonicalised => "sitemap_url_canonicalised", Low,
        "the sitemap lists {n} page whose canonical tag names another URL",
        "the sitemap lists {n} pages whose canonical tag names another URL";
    SitemapMissing => "sitemap_missing", Low,
        "no sitemap was found", "no sitemap was found";
    TitleMissing => "title_missing", Medium,
        "{n} page has no title", "{n} pages have no title";
    TitleDuplicate => "title_duplicate", Low,
        "{n} page shares its title with another page", "{n} pages share their title with another page";
    H1Missing => "h1_missing", Low,
        "{n} page has no H1 heading", "{n} pages have no H1 heading";
    StructuredDataInvalid => "structured_data_invalid", High,
        "{n} page has invalid JSON-LD", "{n} pages have invalid JSON-LD";
    HreflangNoReturn => "hreflang_no_return", Medium,
        "{n} hreflang link has no return link", "{n} hreflang links have no return link";
    HreflangBroken => "hreflang_broken", Medium,
        "{n} hreflang link points to a broken page", "{n} hreflang links point to a broken page";
    HomeNoindex => "home_noindex", Critical,
        "the start page has noindex", "the start page has noindex";
    HomeError => "home_error", Critical,
        "the start page does not load", "the start page does not load";
    RobotsBlocksSite => "robots_blocks_site", Critical,
        "robots.txt blocks Googlebot from the start page", "robots.txt blocks Googlebot from the start page";
    OrphanPage => "orphan_page", Low,
        "{n} page in the sitemap cannot be reached through links",
        "{n} pages in the sitemap cannot be reached through links";

    // Rules from auditroo.toml and quality budgets.
    Assertion => "assertion", Medium,
        "{n} custom check failed{group}", "{n} custom checks failed{group}";
    QualityBudget => "quality_budget", Medium,
        "{n} quality budget failed{group}", "{n} quality budgets failed{group}";
}

impl Rule {
    /// The first sentence of the advice, for one line under a failing
    /// finding in the text output and the job summary.
    pub fn short_advice(self) -> &'static str {
        let advice = self.advice();
        match advice.find(". ") {
            Some(end) => &advice[..=end],
            None => advice,
        }
    }

    /// What to do about a finding, in one or two sentences. The first
    /// sentence says what to do and works on its own.
    pub fn advice(self) -> &'static str {
        match self {
            Rule::NoindexAdded => "Remove the noindex from the robots meta tag or the X-Robots-Tag header, unless you want the page out of Google. A page with noindex drops out of the search results after Google's next visit.",
            Rule::NoindexRemoved => "Check that the page should appear in search results. If it was hidden on purpose, add the noindex back.",
            Rule::PageBroken => "Fix the page, or redirect its URL to the page that replaces it and update the links that point to it.",
            Rule::PageRedirected => "If many pages redirect to the same URL, such as a login page, make the deployment public for auditroo. If the page moved on purpose, use a permanent redirect (301 or 308) and link to the new URL.",
            Rule::FileBroken => "Restore the file, or change the pages that use it. A missing image or PDF shows up as broken on the page.",
            Rule::PageNotHtml => "Serve the HTML page at this URL again. Google drops the page from search while it gets JSON or a file here. A routing rule or a wrong Content-Type header is the usual cause.",
            Rule::RedirectLoop => "Fix the redirect rules so the URL ends on a page that loads. Browsers and Google give up on loops.",
            Rule::RedirectChanged => "Check that the redirect still goes where you want, in as few steps as possible.",
            Rule::FetchFailed => "Check that the server answers this URL. Timeouts can be temporary, so run the check again first.",
            Rule::CanonicalExternal => "Point the canonical tag at the page's own URL. A canonical tag on another host, such as a staging server, tells Google to show that host instead. A wrong site URL in the build is the usual cause. If you checked a preview or a local build of your live site, pass --production-url with the live site, or use compare. If the other host is right, for example for articles that first appeared there, add an ignore rule for canonical_external and these URLs.",
            Rule::CanonicalChanged => "Make sure the canonical tag names the URL you want in search results. When it names another page, Google indexes that page instead.",
            Rule::CanonicalRemoved => "Add the canonical tag back. Without it, Google picks the URL it thinks is best.",
            Rule::TitleChanged => "Check that the new title still describes the page. The title is the headline of the search result.",
            Rule::TitleRemoved | Rule::TitleMissing => "Add a <title> to the page. It is the headline of the search result.",
            Rule::DescriptionChanged | Rule::DescriptionRemoved => "Make sure the page has a meta description that describes it. Google often shows it under the title.",
            Rule::H1Changed => "Check that the new heading still matches what the page is about.",
            Rule::H1Removed | Rule::H1Missing => "Add a main <h1> heading that says what the page is about.",
            Rule::HeadingsChanged => "Nothing to do, unless you did not plan this change. It is usually a content edit.",
            Rule::StructuredDataRemoved => "Add the JSON-LD back if the page should still get rich results in Google, such as prices or breadcrumbs.",
            Rule::StructuredDataChanged => "Check that the page still has the schema.org types it should have.",
            Rule::HreflangChanged => "Check that every language version still lists all versions, including itself.",
            Rule::Orphaned => "Link to the page again from a related page or from the navigation. Google finds pages mainly through links.",
            Rule::SitemapUrlsRemoved => "Add the pages back to the sitemap, or check why the sitemap generator no longer sees them.",
            Rule::SitemapBroken => "Make the sitemap load with status 200 as valid XML, with a full URL in each <loc>. Until then Google cannot read the URLs in it.",
            Rule::RobotsBlocked => "Remove or narrow the Disallow rule in robots.txt. Blocked pages cannot be crawled and drop out of search results over time.",
            Rule::RobotsError => "Make robots.txt load again with status 200, or 404 if you have none. While it returns a server error, Google stops crawling the site.",
            Rule::PageAdded => "Nothing to do. New pages should be in the sitemap and linked from the site.",
            Rule::PageFixed => "Nothing to do.",
            Rule::BrokenLink => "Fix the linked page, or change the links to point to a page that works.",
            Rule::RedirectChain => "Link to the final URL directly, and point old URLs straight at their final target.",
            Rule::CanonicalToRedirect => "Use the final URL of the redirect in the canonical tag.",
            Rule::CanonicalToError => "Point the canonical tag at a page that loads.",
            Rule::CanonicalToNoindex => "Remove the noindex on the page that the canonical tag names, or change the canonical tag. The two say opposite things.",
            Rule::SitemapUrlError => "Fix the page, or remove its URL from the sitemap.",
            Rule::SitemapUrlRedirect => "List the final URL in the sitemap instead of the one that redirects.",
            Rule::SitemapUrlNoindex => "Remove the noindex or remove the page from the sitemap. The two say opposite things.",
            Rule::SitemapUrlBlocked => "Allow the URL in robots.txt or remove it from the sitemap.",
            Rule::SitemapUrlCanonicalised => "List the URL that the canonical tag names in the sitemap instead.",
            Rule::SitemapUrlExternal => "List only this site's own URLs in the sitemap. Google ignores URLs on other hosts here. A staging host in the sitemap usually means the build used the wrong site URL.",
            Rule::SitemapMissing => "Add a sitemap at /sitemap.xml, or name it in robots.txt with a Sitemap: line.",
            Rule::TitleDuplicate => "Give each page its own title so search results can tell them apart.",
            Rule::StructuredDataInvalid => "Fix the JSON syntax. Google ignores JSON-LD it cannot parse.",
            Rule::HreflangNoReturn => "Add the return link on the other page. Google ignores hreflang pairs that do not link both ways.",
            Rule::HreflangBroken => "Point the hreflang link at a page that loads without a redirect.",
            Rule::HomeNoindex => "Remove the noindex from the start page, unless the whole site should stay out of search results.",
            Rule::HomeError => "Fix the start page. Google reaches most of the site through it.",
            Rule::RobotsBlocksSite => "Remove the rule in robots.txt that blocks Googlebot from the start page.",
            Rule::OrphanPage => "Link to the page from the site, or remove it from the sitemap if it is no longer needed.",
            Rule::Assertion => "Compare what the check found with what it expects. The check comes from your auditroo.toml.",
            Rule::QualityBudget => "Compare the measured value with the budget in your auditroo.toml. The value comes from a quality tool such as Lighthouse.",
        }
    }

    pub fn from_id(id: &str) -> Option<Rule> {
        Rule::ALL.iter().copied().find(|rule| rule.id() == id)
    }

    /// One summary line for `count` findings that share `group`.
    pub fn summary(self, count: usize, group: Option<&str>) -> String {
        let group_text = match (self, group) {
            (Rule::RobotsBlocked, Some(group)) => group.to_owned(),
            (Rule::RobotsBlocked, None) => "crawlers".to_owned(),
            (Rule::PageRedirected | Rule::SitemapUrlRedirect, Some(group)) => {
                format!(" to {group}")
            }
            (_, Some(group)) if !group.is_empty() => format!(" ({group})"),
            _ => String::new(),
        };
        self.phrase(count)
            .replace("{n}", &format_count(count))
            .replace("{group}", &group_text)
    }

    /// How the related URLs of a finding are introduced, for example
    /// "linked from".
    pub fn related_label(self) -> &'static str {
        match self {
            Rule::BrokenLink
            | Rule::RedirectChain
            | Rule::PageBroken
            | Rule::FileBroken
            | Rule::RedirectLoop => "linked from",
            Rule::SitemapUrlsRemoved => "removed",
            Rule::RobotsBlocked => "blocked",
            Rule::TitleDuplicate => "same title as",
            Rule::Orphaned => "was linked from",
            _ => "see",
        }
    }

    /// Findings of this rule are about the whole site, not one page.
    pub fn is_site_wide(self) -> bool {
        matches!(
            self,
            Rule::SitemapUrlsRemoved | Rule::RobotsBlocked | Rule::SitemapMissing
        )
    }
}

impl fmt::Display for Rule {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.id())
    }
}

/// Whether a run passed its fail-on level. Log analysis and the monitor
/// history use it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateDecision {
    Passed,
    Failed,
}

/// `Failed` when `effective` reaches `threshold`.
pub fn gate_decision(effective: Option<Severity>, threshold: Severity) -> GateDecision {
    match effective {
        Some(severity) if severity >= threshold => GateDecision::Failed,
        _ => GateDecision::Passed,
    }
}

/// One problem or change, about one URL or about the whole site.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    pub rule: Rule,
    pub severity: Severity,
    /// The page or file this is about, such as robots.txt for
    /// `robots_blocked`. Missing for `sitemap_urls_removed` and
    /// `sitemap_missing`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// One plain sentence, without the URL.
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    /// Related URLs, for example the pages that link to a broken URL. At most
    /// `MAX_RELATED` are kept; `related_total` has the full count.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related: Vec<String>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub related_total: usize,
    /// Shared detail for findings that are summed up together, for example
    /// the host a canonical tag now points to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Extra lines for people, for example which crawlers a robots.txt rule
    /// blocks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<String>,
    /// Changes on the same page that follow from this one and are not
    /// listed separately.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub follow_on: Vec<Rule>,
    /// Id of the custom assertion or quality budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_id: Option<String>,
    /// An ignore rule matched. Always written, as `false` too, so scripts
    /// can filter on it without a default.
    #[serde(default)]
    pub ignored: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub impact: Vec<ImpactAnnotation>,
}

pub const MAX_RELATED: usize = 50;

fn is_zero(value: &usize) -> bool {
    *value == 0
}

impl Finding {
    pub fn new(rule: Rule, url: Option<&str>, message: impl Into<String>) -> Self {
        Self {
            rule,
            severity: rule.severity(),
            url: url.map(str::to_owned),
            message: message.into(),
            before: None,
            after: None,
            related: Vec::new(),
            related_total: 0,
            group: None,
            details: Vec::new(),
            follow_on: Vec::new(),
            check_id: None,
            ignored: false,
            impact: Vec::new(),
        }
    }

    pub fn severity(mut self, severity: Severity) -> Self {
        self.severity = severity;
        self
    }

    pub fn change(mut self, before: Option<String>, after: Option<String>) -> Self {
        self.before = before;
        self.after = after;
        self
    }

    pub fn detail(mut self, line: impl Into<String>) -> Self {
        self.details.push(line.into());
        self
    }

    pub fn group(mut self, group: impl Into<String>) -> Self {
        self.group = Some(group.into());
        self
    }

    pub fn related(mut self, urls: impl IntoIterator<Item = String>) -> Self {
        let mut all = urls.into_iter().collect::<Vec<_>>();
        all.sort();
        all.dedup();
        self.related_total = all.len();
        all.truncate(MAX_RELATED);
        self.related = all;
        self
    }

    /// How many things this finding stands for: the related URLs of a
    /// site-wide finding, one otherwise.
    pub fn weight(&self) -> usize {
        if self.rule.is_site_wide() && self.related_total > 0 {
            self.related_total
        } else {
            1
        }
    }
}

/// "1,234" style numbers.
pub fn format_count(count: usize) -> String {
    let digits = count.to_string();
    let mut out = String::new();
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_ids_round_trip_and_are_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for rule in Rule::ALL {
            assert!(seen.insert(rule.id()), "duplicate id {}", rule.id());
            assert_eq!(Rule::from_id(rule.id()), Some(*rule));
            let json = serde_json::to_string(rule).unwrap();
            assert_eq!(json, format!("\"{}\"", rule.id()));
        }
    }

    #[test]
    fn summaries_read_as_sentences() {
        assert_eq!(
            Rule::NoindexAdded.summary(1, None),
            "1 page now has noindex"
        );
        assert_eq!(
            Rule::NoindexAdded.summary(2, None),
            "2 pages now have noindex"
        );
        assert_eq!(
            Rule::CanonicalExternal.summary(23, Some("staging.example.com")),
            "23 canonical tags point to another host (staging.example.com)"
        );
        assert_eq!(
            Rule::RobotsBlocked.summary(2, Some("Googlebot")),
            "robots.txt now blocks Googlebot from 2 pages"
        );
        assert_eq!(format_count(1234567), "1,234,567");
    }
}
