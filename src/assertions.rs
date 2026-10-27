//! Custom checks. An `[[extract]]` block reads a value from each page, and an
//! `[[assert]]` block says what that value must look like. A check that
//! fails becomes an `assertion` finding.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use regex::Regex;
use scraper::{ElementRef, Html, Selector};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Number, Value};
use url::Url;

use crate::crawl::url::path_and_query;
use crate::crawl::{normalize_url, CrawlItem, RedirectHop};
use crate::extract::{JsonLdBlock, SeoRecord};
use crate::findings::Severity;
use crate::snapshot::Snapshot;

pub const DEFAULT_MAX_VALUES: usize = 20;
pub const DEFAULT_MAX_SCALAR_CHARS: usize = 1024;
pub const DEFAULT_EVIDENCE_CHARS: usize = 160;
pub const MAX_VALUES_LIMIT: usize = 100;
pub const MAX_SCALAR_CHARS_LIMIT: usize = 8192;
pub const MAX_EVIDENCE_CHARS_LIMIT: usize = 1024;
pub const MAX_SERIALIZED_VALUE_BYTES: usize = 8192;
pub const MAX_TOTAL_EXTRACTION_BYTES_PER_URL: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleScope {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
}

impl Default for RuleScope {
    fn default() -> Self {
        Self {
            include: vec!["/*".to_owned()],
            exclude: Vec::new(),
        }
    }
}

impl RuleScope {
    pub fn matches_url(&self, normalized_url: &str) -> bool {
        let Ok(url) = Url::parse(normalized_url) else {
            return false;
        };
        self.matches_path(&path_and_query(&url))
    }

    pub fn matches_path(&self, path: &str) -> bool {
        let included = self
            .include
            .iter()
            .any(|pattern| crate::crawl::ScopePattern::compile(pattern).matches(path));
        let excluded = self
            .exclude
            .iter()
            .any(|pattern| crate::crawl::ScopePattern::compile(pattern).matches(path));
        included && !excluded
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedExtractor {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub scope: RuleScope,
    pub max_values: usize,
    pub max_scalar_chars: usize,
    pub evidence_chars: usize,
    pub definition: ExtractorDefinition,
    #[serde(skip)]
    compiled: CompiledExtractor,
}

impl ResolvedExtractor {
    pub fn new(
        id: String,
        description: Option<String>,
        scope: RuleScope,
        max_values: usize,
        max_scalar_chars: usize,
        evidence_chars: usize,
        definition: ExtractorDefinition,
    ) -> Result<Self, String> {
        let compiled = CompiledExtractor::compile(&definition)?;
        Ok(Self {
            id,
            description,
            scope,
            max_values,
            max_scalar_chars,
            evidence_chars,
            definition,
            compiled,
        })
    }

    pub fn kind(&self) -> ExtractorKind {
        self.definition.kind()
    }

    pub fn output_shape(&self) -> TargetShape {
        match &self.definition {
            ExtractorDefinition::CssText { .. }
            | ExtractorDefinition::CssAttr { .. }
            | ExtractorDefinition::Regex { .. }
            | ExtractorDefinition::Header { .. } => TargetShape::StringList { url_valued: false },
            ExtractorDefinition::CssCount { .. } => TargetShape::Count,
            ExtractorDefinition::JsonLd { .. } => TargetShape::JsonObjectList,
            ExtractorDefinition::JsonPointer { .. } => TargetShape::JsonList,
            ExtractorDefinition::UrlPart { part } => TargetShape::OptionalString {
                url_valued: *part == UrlPart::NormalizedUrl,
            },
        }
    }
}

impl PartialEq for ResolvedExtractor {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.description == other.description
            && self.scope == other.scope
            && self.max_values == other.max_values
            && self.max_scalar_chars == other.max_scalar_chars
            && self.evidence_chars == other.evidence_chars
            && self.definition == other.definition
    }
}

#[derive(Clone, Default)]
enum CompiledExtractor {
    CssText(Arc<Selector>),
    CssAttr(Arc<Selector>),
    CssCount(Arc<Selector>),
    Regex(Arc<Regex>),
    #[default]
    None,
}

impl fmt::Debug for CompiledExtractor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::CssText(_) => "CssText",
            Self::CssAttr(_) => "CssAttr",
            Self::CssCount(_) => "CssCount",
            Self::Regex(_) => "Regex",
            Self::None => "None",
        })
    }
}

impl CompiledExtractor {
    fn compile(definition: &ExtractorDefinition) -> Result<Self, String> {
        match definition {
            ExtractorDefinition::CssText { selector } => Selector::parse(selector)
                .map(|selector| Self::CssText(Arc::new(selector)))
                .map_err(|_| "the CSS selector is not valid".to_owned()),
            ExtractorDefinition::CssAttr { selector, .. } => Selector::parse(selector)
                .map(|selector| Self::CssAttr(Arc::new(selector)))
                .map_err(|_| "the CSS selector is not valid".to_owned()),
            ExtractorDefinition::CssCount { selector } => Selector::parse(selector)
                .map(|selector| Self::CssCount(Arc::new(selector)))
                .map_err(|_| "the CSS selector is not valid".to_owned()),
            ExtractorDefinition::Regex { pattern, .. } => Regex::new(pattern)
                .map(|regex| Self::Regex(Arc::new(regex)))
                .map_err(|_| "the regex is not valid".to_owned()),
            _ => Ok(Self::None),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExtractorDefinition {
    CssText {
        selector: String,
    },
    CssAttr {
        selector: String,
        attr: String,
    },
    CssCount {
        selector: String,
    },
    Regex {
        source: RegexSource,
        pattern: String,
        group: usize,
    },
    JsonLd {
        #[serde(skip_serializing_if = "Option::is_none")]
        schema_type: Option<String>,
    },
    JsonPointer {
        pointer: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        schema_type: Option<String>,
    },
    Header {
        name: String,
    },
    UrlPart {
        part: UrlPart,
    },
}

impl ExtractorDefinition {
    pub fn kind(&self) -> ExtractorKind {
        match self {
            Self::CssText { .. } => ExtractorKind::CssText,
            Self::CssAttr { .. } => ExtractorKind::CssAttr,
            Self::CssCount { .. } => ExtractorKind::CssCount,
            Self::Regex { .. } => ExtractorKind::Regex,
            Self::JsonLd { .. } => ExtractorKind::JsonLd,
            Self::JsonPointer { .. } => ExtractorKind::JsonPointer,
            Self::Header { .. } => ExtractorKind::Header,
            Self::UrlPart { .. } => ExtractorKind::UrlPart,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtractorKind {
    CssText,
    CssAttr,
    CssCount,
    Regex,
    JsonLd,
    JsonPointer,
    Header,
    UrlPart,
}

impl ExtractorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CssText => "css_text",
            Self::CssAttr => "css_attr",
            Self::CssCount => "css_count",
            Self::Regex => "regex",
            Self::JsonLd => "json_ld",
            Self::JsonPointer => "json_pointer",
            Self::Header => "header",
            Self::UrlPart => "url_part",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegexSource {
    Body,
    DomText,
}

impl RegexSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Body => "body",
            Self::DomText => "dom_text",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UrlPart {
    Scheme,
    Host,
    Path,
    Query,
    Extension,
    NormalizedUrl,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CustomExtraction {
    pub catalog: Vec<ResolvedExtractor>,
    pub values: BTreeMap<String, BTreeMap<String, ExtractionResult>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExtractionResult {
    pub state: ExtractionState,
    pub source: ExtractionSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtractionState {
    Value,
    Missing,
    Empty,
    ParseError,
    UnsupportedSource,
    LimitExceeded,
}

impl ExtractionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Value => "value",
            Self::Missing => "missing",
            Self::Empty => "empty",
            Self::ParseError => "parse_error",
            Self::UnsupportedSource => "unsupported_source",
            Self::LimitExceeded => "limit_exceeded",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtractionSource {
    Html,
    Body,
    DomText,
    JsonLd,
    Header,
    Url,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedAssertion {
    pub id: String,
    pub target: AssertionTarget,
    pub operator: AssertionOperator,
    pub severity: Severity,
    pub scope: RuleScope,
    pub description: Option<String>,
    pub message: Option<String>,
    pub condition: AssertionCondition,
}

impl ResolvedAssertion {
    pub fn target_matches_url(
        &self,
        normalized_url: &str,
        extractors: &BTreeMap<String, ResolvedExtractor>,
    ) -> bool {
        if !self.scope.matches_url(normalized_url) {
            return false;
        }
        match &self.target {
            AssertionTarget::Field(_) => true,
            AssertionTarget::Extractor(id) => extractors
                .get(id)
                .is_some_and(|extractor| extractor.scope.matches_url(normalized_url)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssertionTarget {
    Field(BuiltinTarget),
    Extractor(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuiltinTarget {
    Canonical,
    IndexabilityIsIndexable,
    Title,
    MetaDescription,
    HeadingsH1,
    HeadingsH2,
    HeadingsH3,
    StructuredData,
    Hreflang,
    HttpStatus,
    FinalUrl,
    RedirectChain,
}

impl BuiltinTarget {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Canonical => "canonical",
            Self::IndexabilityIsIndexable => "indexability.is_indexable",
            Self::Title => "title",
            Self::MetaDescription => "meta_description",
            Self::HeadingsH1 => "headings.h1",
            Self::HeadingsH2 => "headings.h2",
            Self::HeadingsH3 => "headings.h3",
            Self::StructuredData => "structured_data",
            Self::Hreflang => "hreflang",
            Self::HttpStatus => "http_status",
            Self::FinalUrl => "final_url",
            Self::RedirectChain => "redirect_chain",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssertionOperator {
    Exists,
    Missing,
    Equals,
    NotEquals,
    Contains,
    NotContains,
    Matches,
    NotMatches,
    CountMin,
    CountMax,
    CountBetween,
    NumericMin,
    NumericMax,
    NumericBetween,
    UrlSameHost,
    UrlAllowedHost,
    JsonShape,
}

impl AssertionOperator {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exists => "exists",
            Self::Missing => "missing",
            Self::Equals => "equals",
            Self::NotEquals => "not_equals",
            Self::Contains => "contains",
            Self::NotContains => "not_contains",
            Self::Matches => "matches",
            Self::NotMatches => "not_matches",
            Self::CountMin => "count_min",
            Self::CountMax => "count_max",
            Self::CountBetween => "count_between",
            Self::NumericMin => "numeric_min",
            Self::NumericMax => "numeric_max",
            Self::NumericBetween => "numeric_between",
            Self::UrlSameHost => "url_same_host",
            Self::UrlAllowedHost => "url_allowed_host",
            Self::JsonShape => "json_shape",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum AssertionCondition {
    None,
    Expected(Value),
    Pattern(String),
    CountMin { min: usize },
    CountMax { max: usize },
    CountBetween { min: usize, max: usize },
    NumericMin { min: f64 },
    NumericMax { max: f64 },
    NumericBetween { min: f64, max: f64 },
    AllowedHosts(Vec<String>),
    JsonShape { required: Vec<JsonShapeRequirement> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JsonShapeRequirement {
    pub pointer: String,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub value_type: Option<JsonPrimitiveType>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JsonPrimitiveType {
    String,
    Number,
    Boolean,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetShape {
    OptionalString { url_valued: bool },
    Boolean,
    Integer,
    Count,
    StringList { url_valued: bool },
    JsonList,
    JsonObjectList,
    RedirectChain,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssertionFinding {
    pub id: String,
    pub url: String,
    pub assertion_id: String,
    pub target: AssertionFindingTarget,
    pub operator: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected: Option<Value>,
    pub observed: Value,
    pub failure_reason: String,
    pub evidence: Vec<String>,
    pub severity: Severity,
    pub provenance: AssertionFindingProvenance,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AssertionFindingTarget {
    Field { field: String },
    Extractor { extractor: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssertionFindingProvenance {
    Assertion,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssertionError {
    InvalidRuntime(String),
    ZeroTargetAssertion(String),
}

impl fmt::Display for AssertionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRuntime(detail) => write!(formatter, "a custom check could not run: {detail}"),
            Self::ZeroTargetAssertion(id) => write!(
                formatter,
                "the custom check {id:?} matches no URL of the crawl. Check its include patterns, or run auditroo assert --dry-run"
            ),
        }
    }
}

impl Error for AssertionError {}

pub fn builtin_target_from_str(field: &str) -> Option<BuiltinTarget> {
    match field {
        "canonical" => Some(BuiltinTarget::Canonical),
        "indexability.is_indexable" => Some(BuiltinTarget::IndexabilityIsIndexable),
        "title" => Some(BuiltinTarget::Title),
        "meta_description" => Some(BuiltinTarget::MetaDescription),
        "headings.h1" => Some(BuiltinTarget::HeadingsH1),
        "headings.h2" => Some(BuiltinTarget::HeadingsH2),
        "headings.h3" => Some(BuiltinTarget::HeadingsH3),
        "structured_data" => Some(BuiltinTarget::StructuredData),
        "hreflang" => Some(BuiltinTarget::Hreflang),
        "http_status" => Some(BuiltinTarget::HttpStatus),
        "final_url" => Some(BuiltinTarget::FinalUrl),
        "redirect_chain" => Some(BuiltinTarget::RedirectChain),
        _ => None,
    }
}

pub fn builtin_target_shape(target: &BuiltinTarget) -> Option<TargetShape> {
    Some(match target {
        BuiltinTarget::Canonical => TargetShape::OptionalString { url_valued: true },
        BuiltinTarget::IndexabilityIsIndexable => TargetShape::Boolean,
        BuiltinTarget::Title | BuiltinTarget::MetaDescription => {
            TargetShape::OptionalString { url_valued: false }
        }
        BuiltinTarget::HeadingsH1 | BuiltinTarget::HeadingsH2 | BuiltinTarget::HeadingsH3 => {
            TargetShape::StringList { url_valued: false }
        }
        BuiltinTarget::StructuredData | BuiltinTarget::Hreflang => TargetShape::JsonList,
        BuiltinTarget::HttpStatus => TargetShape::Integer,
        BuiltinTarget::FinalUrl => TargetShape::OptionalString { url_valued: true },
        BuiltinTarget::RedirectChain => TargetShape::RedirectChain,
    })
}

pub fn extract_custom_values(
    item: &CrawlItem,
    extractors: &[ResolvedExtractor],
) -> BTreeMap<String, ExtractionResult> {
    let mut results = BTreeMap::new();
    if extractors.is_empty() {
        return results;
    }

    let normalized_url = normalize_crawl_identity(&item.url);
    let html_supported = supports_html(item.response.content_type.as_deref());
    let document = html_supported.then(|| Html::parse_document(&item.response.body));
    let dom_text = document.as_ref().map(document_text_without_code);

    for extractor in extractors {
        if !extractor.scope.matches_url(&normalized_url) {
            continue;
        }
        let result = evaluate_extractor(item, extractor, document.as_ref(), dom_text.as_deref());
        results.insert(extractor.id.clone(), result);
    }

    results
}

fn evaluate_extractor(
    item: &CrawlItem,
    extractor: &ResolvedExtractor,
    document: Option<&Html>,
    dom_text: Option<&str>,
) -> ExtractionResult {
    match &extractor.definition {
        ExtractorDefinition::CssText { selector } => {
            let Some(document) = document else {
                return unsupported_html(item, ExtractionSource::Html);
            };
            let Ok((values, matched_any)) =
                with_css_selector(extractor, selector, ExtractionSource::Html, |selector| {
                    let mut values = Vec::new();
                    let mut matched_any = false;
                    for element in document.select(selector) {
                        matched_any = true;
                        values.push(collapse_whitespace(&element_text(&element)));
                        if values.len() > extractor.max_values {
                            break;
                        }
                    }
                    (values, matched_any)
                })
            else {
                return limit_or_parse_error(
                    ExtractionSource::Html,
                    "the CSS selector is not valid",
                );
            };
            string_list_result(
                values,
                matched_any,
                ExtractionSource::Html,
                extractor,
                EmptyBehavior::AllEmptyIsEmpty,
            )
        }
        ExtractorDefinition::CssAttr { selector, attr } => {
            let Some(document) = document else {
                return unsupported_html(item, ExtractionSource::Html);
            };
            let Ok(values) =
                with_css_selector(extractor, selector, ExtractionSource::Html, |selector| {
                    let mut values = Vec::new();
                    for element in document.select(selector) {
                        if let Some(value) = element.value().attr(attr) {
                            values.push(value.to_owned());
                            if values.len() > extractor.max_values {
                                break;
                            }
                        }
                    }
                    values
                })
            else {
                return limit_or_parse_error(
                    ExtractionSource::Html,
                    "the CSS selector is not valid",
                );
            };
            string_list_result(
                values,
                false,
                ExtractionSource::Html,
                extractor,
                EmptyBehavior::AllEmptyIsEmpty,
            )
        }
        ExtractorDefinition::CssCount { selector } => {
            let Some(document) = document else {
                return unsupported_html(item, ExtractionSource::Html);
            };
            let Ok(count) =
                with_css_selector(extractor, selector, ExtractionSource::Html, |selector| {
                    document.select(selector).count()
                })
            else {
                return limit_or_parse_error(
                    ExtractionSource::Html,
                    "the CSS selector is not valid",
                );
            };
            value_result(json!(count), ExtractionSource::Html, Vec::new(), extractor)
        }
        ExtractorDefinition::Regex {
            source,
            pattern,
            group,
        } => {
            let haystack = match source {
                RegexSource::Body => item.response.body.as_str(),
                RegexSource::DomText => {
                    let Some(text) = dom_text else {
                        return unsupported_html(item, ExtractionSource::DomText);
                    };
                    text
                }
            };
            let source = match source {
                RegexSource::Body => ExtractionSource::Body,
                RegexSource::DomText => ExtractionSource::DomText,
            };
            let Ok(values) = with_regex(extractor, pattern, source, |regex| {
                let mut values = Vec::new();
                for captures in regex.captures_iter(haystack) {
                    if let Some(matched) = captures.get(*group) {
                        values.push(matched.as_str().to_owned());
                        if values.len() > extractor.max_values {
                            break;
                        }
                    }
                }
                values
            }) else {
                return limit_or_parse_error(source, "the regex is not valid");
            };
            string_list_result(
                values,
                false,
                source,
                extractor,
                EmptyBehavior::AllEmptyIsEmpty,
            )
        }
        ExtractorDefinition::JsonLd { schema_type } => {
            let Some(document) = document else {
                return unsupported_html(item, ExtractionSource::JsonLd);
            };
            match json_ld_nodes(document, schema_type.as_deref()) {
                Ok(nodes) => json_list_result(nodes, ExtractionSource::JsonLd, extractor, true),
                Err(detail) => parse_error(ExtractionSource::JsonLd, detail),
            }
        }
        ExtractorDefinition::JsonPointer {
            pointer,
            schema_type,
        } => {
            let Some(document) = document else {
                return unsupported_html(item, ExtractionSource::JsonLd);
            };
            match json_ld_nodes(document, schema_type.as_deref()) {
                Ok(nodes) => {
                    let values = nodes
                        .iter()
                        .filter_map(|node| node.pointer(pointer).cloned())
                        .collect::<Vec<_>>();
                    json_list_result(values, ExtractionSource::JsonLd, extractor, false)
                }
                Err(detail) => parse_error(ExtractionSource::JsonLd, detail),
            }
        }
        ExtractorDefinition::Header { name } => {
            let values = item
                .response
                .headers
                .get_all_case_insensitive(name)
                .to_vec();
            string_list_result(
                values,
                false,
                ExtractionSource::Header,
                extractor,
                EmptyBehavior::AllEmptyIsEmpty,
            )
        }
        ExtractorDefinition::UrlPart { part } => url_part_result(&item.url, *part, extractor),
    }
}

fn with_css_selector<T>(
    extractor: &ResolvedExtractor,
    raw_selector: &str,
    source: ExtractionSource,
    evaluate: impl FnOnce(&Selector) -> T,
) -> Result<T, ExtractionResult> {
    if let Some(selector) = cached_css_selector(extractor) {
        return Ok(evaluate(selector));
    }
    let selector = Selector::parse(raw_selector)
        .map_err(|_| limit_or_parse_error(source, "the CSS selector is not valid"))?;
    Ok(evaluate(&selector))
}

fn cached_css_selector(extractor: &ResolvedExtractor) -> Option<&Selector> {
    match (&extractor.definition, &extractor.compiled) {
        (ExtractorDefinition::CssText { .. }, CompiledExtractor::CssText(selector))
        | (ExtractorDefinition::CssAttr { .. }, CompiledExtractor::CssAttr(selector))
        | (ExtractorDefinition::CssCount { .. }, CompiledExtractor::CssCount(selector)) => {
            Some(selector.as_ref())
        }
        _ => None,
    }
}

fn with_regex<T>(
    extractor: &ResolvedExtractor,
    raw_pattern: &str,
    source: ExtractionSource,
    evaluate: impl FnOnce(&Regex) -> T,
) -> Result<T, ExtractionResult> {
    if let (ExtractorDefinition::Regex { .. }, CompiledExtractor::Regex(regex)) =
        (&extractor.definition, &extractor.compiled)
    {
        return Ok(evaluate(regex.as_ref()));
    }
    let regex = Regex::new(raw_pattern)
        .map_err(|_| limit_or_parse_error(source, "the regex is not valid"))?;
    Ok(evaluate(&regex))
}

pub fn evaluate_assertions(
    baseline: &Snapshot,
    current: &Snapshot,
    extractors: &[ResolvedExtractor],
    assertions: &[ResolvedAssertion],
) -> Result<Vec<AssertionFinding>, AssertionError> {
    if assertions.is_empty() {
        return Ok(Vec::new());
    }

    let extractor_map = extractors
        .iter()
        .cloned()
        .map(|extractor| (extractor.id.clone(), extractor))
        .collect::<BTreeMap<_, _>>();
    validate_zero_target_assertions(baseline, current, &extractor_map, assertions)?;

    let mut findings = Vec::new();
    for record in current.records.values() {
        for assertion in assertions {
            if !assertion.target_matches_url(&record.url, &extractor_map) {
                continue;
            }
            let observed = observed_value(record, current, assertion)?;
            if let Some(finding) = evaluate_one_assertion(assertion, record, observed)? {
                findings.push(finding);
            }
        }
    }

    findings.sort_by(|left, right| {
        (&left.url, &left.assertion_id, &left.id).cmp(&(&right.url, &right.assertion_id, &right.id))
    });
    Ok(findings)
}

fn validate_zero_target_assertions(
    baseline: &Snapshot,
    current: &Snapshot,
    extractors: &BTreeMap<String, ResolvedExtractor>,
    assertions: &[ResolvedAssertion],
) -> Result<(), AssertionError> {
    let known = known_urls_for_assertions(baseline, current);
    for assertion in assertions {
        if !known
            .iter()
            .any(|url| assertion.target_matches_url(url, extractors))
        {
            return Err(AssertionError::ZeroTargetAssertion(assertion.id.clone()));
        }
    }
    Ok(())
}

pub fn known_urls_for_assertions(baseline: &Snapshot, current: &Snapshot) -> BTreeSet<String> {
    let mut urls = BTreeSet::new();
    urls.extend(baseline.records.keys().cloned());
    urls.extend(current.records.keys().cloned());
    urls.extend(current.crawl_failures.keys().cloned());
    urls.extend(current.crawl_observation.skipped_candidates.keys().cloned());
    urls
}

fn observed_value(
    record: &SeoRecord,
    snapshot: &Snapshot,
    assertion: &ResolvedAssertion,
) -> Result<RuntimeValue, AssertionError> {
    match &assertion.target {
        AssertionTarget::Field(field) => Ok(observed_builtin(record, *field)),
        AssertionTarget::Extractor(id) => {
            let Some(custom) = &snapshot.custom_extraction else {
                return Err(AssertionError::InvalidRuntime(format!(
                    "the crawl has no values for the extractor {id:?}"
                )));
            };
            let Some(per_url) = custom.values.get(&record.url) else {
                return Err(AssertionError::InvalidRuntime(format!(
                    "the crawl has no extractor values for {}",
                    record.url
                )));
            };
            let Some(result) = per_url.get(id) else {
                return Err(AssertionError::InvalidRuntime(format!(
                    "the crawl has no value of the extractor {id:?} for {}",
                    record.url
                )));
            };
            Ok(RuntimeValue::from_extraction(result))
        }
    }
}

fn observed_builtin(record: &SeoRecord, target: BuiltinTarget) -> RuntimeValue {
    match target {
        BuiltinTarget::Canonical => optional_string_runtime(record.canonical.clone(), Vec::new()),
        BuiltinTarget::IndexabilityIsIndexable => {
            RuntimeValue::value(json!(record.indexability.is_indexable), Vec::new())
        }
        BuiltinTarget::Title => optional_string_runtime(record.title.clone(), Vec::new()),
        BuiltinTarget::MetaDescription => {
            optional_string_runtime(record.meta_description.clone(), Vec::new())
        }
        BuiltinTarget::HeadingsH1 => string_list_runtime(&record.headings.h1),
        BuiltinTarget::HeadingsH2 => string_list_runtime(&record.headings.h2),
        BuiltinTarget::HeadingsH3 => string_list_runtime(&record.headings.h3),
        BuiltinTarget::StructuredData => {
            let value = Value::Array(
                record
                    .structured_data
                    .iter()
                    .map(json_ld_block_value)
                    .collect(),
            );
            RuntimeValue::with_state(
                if record.structured_data.is_empty() {
                    RuntimeState::Empty
                } else {
                    RuntimeState::Value
                },
                Some(value),
                evidence_from_json_blocks(&record.structured_data),
                None,
            )
        }
        BuiltinTarget::Hreflang => {
            let value = serde_json::to_value(&record.hreflang).unwrap_or(Value::Array(Vec::new()));
            RuntimeValue::with_state(
                if record.hreflang.is_empty() {
                    RuntimeState::Empty
                } else {
                    RuntimeState::Value
                },
                Some(value),
                Vec::new(),
                None,
            )
        }
        BuiltinTarget::HttpStatus => RuntimeValue::value(json!(record.http_status), Vec::new()),
        BuiltinTarget::FinalUrl => RuntimeValue::value(json!(record.final_url), Vec::new()),
        BuiltinTarget::RedirectChain => {
            let value =
                serde_json::to_value(&record.redirect_chain).unwrap_or(Value::Array(Vec::new()));
            RuntimeValue::with_state(
                if record.redirect_chain.is_empty() {
                    RuntimeState::Empty
                } else {
                    RuntimeState::Value
                },
                Some(value),
                redirect_evidence(&record.redirect_chain),
                None,
            )
        }
    }
}

fn evaluate_one_assertion(
    assertion: &ResolvedAssertion,
    record: &SeoRecord,
    observed: RuntimeValue,
) -> Result<Option<AssertionFinding>, AssertionError> {
    let outcome = assertion_passes(assertion, record, &observed)?;
    if outcome.passed {
        return Ok(None);
    }

    let observed_summary = observed_summary(&observed);
    let expected = expected_summary(assertion);
    let target = finding_target(&assertion.target);
    let evidence = observed
        .evidence
        .iter()
        .map(|value| bounded_line(value, DEFAULT_EVIDENCE_CHARS))
        .take(DEFAULT_MAX_VALUES)
        .collect::<Vec<_>>();

    Ok(Some(AssertionFinding {
        id: format!("assertion::{}::{}", assertion.id, record.url),
        url: record.url.clone(),
        assertion_id: assertion.id.clone(),
        target,
        operator: assertion.operator.as_str().to_owned(),
        expected,
        observed: observed_summary,
        failure_reason: bounded_line(
            assertion
                .message
                .as_deref()
                .filter(|message| !message.is_empty())
                .unwrap_or(&outcome.failure_reason),
            240,
        ),
        evidence,
        severity: assertion.severity,
        provenance: AssertionFindingProvenance::Assertion,
    }))
}

#[derive(Debug, Clone)]
struct RuntimeValue {
    state: RuntimeState,
    value: Option<Value>,
    evidence: Vec<String>,
    detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeState {
    Value,
    Missing,
    Empty,
    Error(ExtractionState),
}

impl RuntimeValue {
    fn value(value: Value, evidence: Vec<String>) -> Self {
        Self::with_state(RuntimeState::Value, Some(value), evidence, None)
    }

    fn with_state(
        state: RuntimeState,
        value: Option<Value>,
        evidence: Vec<String>,
        detail: Option<String>,
    ) -> Self {
        Self {
            state,
            value,
            evidence,
            detail,
        }
    }

    fn from_extraction(result: &ExtractionResult) -> Self {
        let state = match result.state {
            ExtractionState::Value => RuntimeState::Value,
            ExtractionState::Missing => RuntimeState::Missing,
            ExtractionState::Empty => RuntimeState::Empty,
            error => RuntimeState::Error(error),
        };
        Self::with_state(
            state,
            result.value.clone(),
            result.evidence.clone(),
            result.detail.clone(),
        )
    }
}

struct AssertionOutcome {
    passed: bool,
    failure_reason: String,
}

fn assertion_passes(
    assertion: &ResolvedAssertion,
    record: &SeoRecord,
    observed: &RuntimeValue,
) -> Result<AssertionOutcome, AssertionError> {
    match assertion.operator {
        AssertionOperator::Exists => {
            return Ok(outcome(
                !matches!(
                    observed.state,
                    RuntimeState::Missing | RuntimeState::Error(_)
                ),
                observed_failure_reason(observed, "value is missing"),
            ));
        }
        AssertionOperator::Missing => {
            return Ok(outcome(
                observed.state == RuntimeState::Missing,
                "value is present".to_owned(),
            ));
        }
        _ => {}
    }

    if let RuntimeState::Missing = observed.state {
        return Ok(outcome(false, "value is missing".to_owned()));
    }
    if let RuntimeState::Error(state) = observed.state {
        return Ok(outcome(
            false,
            format!(
                "the extractor could not read the value ({})",
                state.as_str()
            ),
        ));
    }

    let value = observed.value.as_ref().ok_or_else(|| {
        AssertionError::InvalidRuntime("the check found a value but could not read it".to_owned())
    })?;

    let evaluated = (|| -> Result<bool, AssertionError> {
        Ok(match (&assertion.operator, &assertion.condition) {
            (AssertionOperator::Equals, AssertionCondition::Expected(expected)) => {
                value == expected
            }
            (AssertionOperator::NotEquals, AssertionCondition::Expected(expected)) => {
                value != expected
            }
            (AssertionOperator::Contains, AssertionCondition::Expected(expected)) => {
                contains_value(value, expected)?
            }
            (AssertionOperator::NotContains, AssertionCondition::Expected(expected)) => {
                !contains_value(value, expected)?
            }
            (AssertionOperator::Matches, AssertionCondition::Pattern(pattern)) => {
                regex_matches(value, pattern, true)?
            }
            (AssertionOperator::NotMatches, AssertionCondition::Pattern(pattern)) => {
                regex_matches(value, pattern, false)?
            }
            (AssertionOperator::CountMin, AssertionCondition::CountMin { min }) => {
                count_value(value)? >= *min
            }
            (AssertionOperator::CountMax, AssertionCondition::CountMax { max }) => {
                count_value(value)? <= *max
            }
            (AssertionOperator::CountBetween, AssertionCondition::CountBetween { min, max }) => {
                let count = count_value(value)?;
                count >= *min && count <= *max
            }
            (AssertionOperator::NumericMin, AssertionCondition::NumericMin { min }) => {
                numeric_value(value)? >= *min
            }
            (AssertionOperator::NumericMax, AssertionCondition::NumericMax { max }) => {
                numeric_value(value)? <= *max
            }
            (
                AssertionOperator::NumericBetween,
                AssertionCondition::NumericBetween { min, max },
            ) => {
                let number = numeric_value(value)?;
                number >= *min && number <= *max
            }
            (AssertionOperator::UrlSameHost, AssertionCondition::None) => {
                urls_same_host(value, &record.final_url)?
            }
            (AssertionOperator::UrlAllowedHost, AssertionCondition::AllowedHosts(hosts)) => {
                urls_allowed_hosts(value, hosts)?
            }
            (AssertionOperator::JsonShape, AssertionCondition::JsonShape { required }) => {
                json_shape_matches(value, required)?
            }
            _ => {
                return Err(AssertionError::InvalidRuntime(format!(
                    "the custom check {} has settings that do not fit its operator",
                    assertion.id
                )));
            }
        })
    })();
    let passed = match evaluated {
        Ok(passed) => passed,
        Err(AssertionError::InvalidRuntime(detail)) if detail.starts_with("type mismatch:") => {
            return Ok(outcome(false, detail));
        }
        Err(error) => return Err(error),
    };

    Ok(outcome(
        passed,
        failure_reason_for(assertion.operator, observed),
    ))
}

fn outcome(passed: bool, failure_reason: String) -> AssertionOutcome {
    AssertionOutcome {
        passed,
        failure_reason,
    }
}

fn contains_value(value: &Value, expected: &Value) -> Result<bool, AssertionError> {
    match value {
        Value::String(text) => {
            let Some(expected) = expected.as_str() else {
                return Err(type_mismatch("contains needs a text value in expected"));
            };
            Ok(text.contains(expected))
        }
        Value::Array(items) => Ok(items.iter().any(|item| item == expected)),
        _ => Err(type_mismatch("contains works only on text or a list")),
    }
}

fn regex_matches(value: &Value, pattern: &str, positive: bool) -> Result<bool, AssertionError> {
    let regex = Regex::new(pattern)
        .map_err(|_| AssertionError::InvalidRuntime("invalid assertion regex".to_owned()))?;
    match value {
        Value::String(text) => {
            let matched = regex.is_match(text);
            Ok(if positive { matched } else { !matched })
        }
        Value::Array(items) => {
            let mut saw_string = false;
            let any_match = items.iter().try_fold(false, |matched, item| {
                let Some(text) = item.as_str() else {
                    return Err(type_mismatch(
                        "matches and not_matches need a list of text values",
                    ));
                };
                saw_string = true;
                Ok(matched || regex.is_match(text))
            })?;
            if !saw_string {
                return Ok(!positive);
            }
            Ok(if positive { any_match } else { !any_match })
        }
        _ => Err(type_mismatch(
            "matches and not_matches work only on text or a list of text",
        )),
    }
}

fn count_value(value: &Value) -> Result<usize, AssertionError> {
    match value {
        Value::Array(items) => Ok(items.len()),
        Value::Number(number) => number
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| type_mismatch("the count must be a whole number of 0 or more")),
        _ => Err(type_mismatch("count checks work only on a list or a count")),
    }
}

fn numeric_value(value: &Value) -> Result<f64, AssertionError> {
    value
        .as_f64()
        .filter(|number| number.is_finite())
        .ok_or_else(|| type_mismatch("number checks need a number"))
}

fn urls_same_host(value: &Value, page_final_url: &str) -> Result<bool, AssertionError> {
    let page_host = absolute_http_host(page_final_url)?;
    Ok(url_values(value)?.iter().all(|url| {
        absolute_http_host(url)
            .map(|host| host == page_host)
            .unwrap_or(false)
    }))
}

fn urls_allowed_hosts(value: &Value, allowed_hosts: &[String]) -> Result<bool, AssertionError> {
    let allowed = allowed_hosts.iter().cloned().collect::<BTreeSet<_>>();
    Ok(url_values(value)?.iter().all(|url| {
        absolute_http_host(url)
            .map(|host| allowed.contains(&host))
            .unwrap_or(false)
    }))
}

fn url_values(value: &Value) -> Result<Vec<String>, AssertionError> {
    match value {
        Value::String(url) => Ok(vec![url.clone()]),
        Value::Array(items) => {
            if items.is_empty() {
                return Ok(Vec::new());
            }
            if items.iter().all(|item| {
                item.as_object()
                    .is_some_and(|object| object.contains_key("to"))
            }) {
                return items
                    .iter()
                    .map(|item| {
                        item.get("to")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                            .ok_or_else(|| type_mismatch("each redirect step needs a URL"))
                    })
                    .collect();
            }
            items
                .iter()
                .map(|item| {
                    item.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| type_mismatch("URL checks need URLs as text"))
                })
                .collect()
        }
        _ => Err(type_mismatch("URL checks need URLs as text")),
    }
}

fn absolute_http_host(raw: &str) -> Result<String, AssertionError> {
    let url = Url::parse(raw).map_err(|_| type_mismatch("the value is not a full URL"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(type_mismatch("the URL is not an http:// or https:// URL"));
    }
    url.host_str()
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| type_mismatch("the URL has no host"))
}

fn json_shape_matches(
    value: &Value,
    required: &[JsonShapeRequirement],
) -> Result<bool, AssertionError> {
    let Value::Array(nodes) = value else {
        return Err(type_mismatch("json_shape needs a list of JSON-LD nodes"));
    };
    if nodes.is_empty() {
        return Ok(false);
    }
    for node in nodes {
        if !node.is_object() {
            return Err(type_mismatch("json_shape needs JSON objects"));
        }
        for requirement in required {
            let Some(found) = node.pointer(&requirement.pointer) else {
                return Ok(false);
            };
            let Some(value_type) = requirement.value_type else {
                continue;
            };
            let type_matches = match value_type {
                JsonPrimitiveType::String => found.is_string(),
                JsonPrimitiveType::Number => found.is_number(),
                JsonPrimitiveType::Boolean => found.is_boolean(),
            };
            if !type_matches {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

fn type_mismatch(detail: &str) -> AssertionError {
    AssertionError::InvalidRuntime(format!("type mismatch: {detail}"))
}

fn failure_reason_for(operator: AssertionOperator, observed: &RuntimeValue) -> String {
    if matches!(observed.state, RuntimeState::Empty) {
        return "value is empty".to_owned();
    }
    match operator {
        AssertionOperator::Equals => "the value is not the expected value",
        AssertionOperator::NotEquals => "the value is the one the check does not allow",
        AssertionOperator::Contains => "the value does not contain the expected value",
        AssertionOperator::NotContains => "the value contains something the check does not allow",
        AssertionOperator::Matches => "the value does not match the pattern",
        AssertionOperator::NotMatches => "the value matches a pattern the check does not allow",
        AssertionOperator::CountMin => "the count is below the minimum",
        AssertionOperator::CountMax => "the count is above the maximum",
        AssertionOperator::CountBetween => "the count is outside the allowed range",
        AssertionOperator::NumericMin => "the number is below the minimum",
        AssertionOperator::NumericMax => "the number is above the maximum",
        AssertionOperator::NumericBetween => "the number is outside the allowed range",
        AssertionOperator::UrlSameHost => "the URL is on another host than the page",
        AssertionOperator::UrlAllowedHost => "the URL is on a host the check does not allow",
        AssertionOperator::JsonShape => "the JSON-LD does not have the required fields",
        AssertionOperator::Exists | AssertionOperator::Missing => "the check failed",
    }
    .to_owned()
}

fn observed_failure_reason(observed: &RuntimeValue, missing_reason: &str) -> String {
    match observed.state {
        RuntimeState::Missing => missing_reason.to_owned(),
        RuntimeState::Error(state) => {
            format!(
                "the extractor could not read the value ({})",
                state.as_str()
            )
        }
        _ => "the check failed".to_owned(),
    }
}

fn expected_summary(assertion: &ResolvedAssertion) -> Option<Value> {
    match &assertion.condition {
        AssertionCondition::None => None,
        AssertionCondition::Expected(value) => Some(bounded_value(value, 20)),
        AssertionCondition::Pattern(pattern) => Some(json!({ "pattern": pattern })),
        AssertionCondition::CountMin { min } => Some(json!({ "min": min })),
        AssertionCondition::CountMax { max } => Some(json!({ "max": max })),
        AssertionCondition::CountBetween { min, max } => Some(json!({ "min": min, "max": max })),
        AssertionCondition::NumericMin { min } => Some(json!({ "min": finite_number_value(*min) })),
        AssertionCondition::NumericMax { max } => Some(json!({ "max": finite_number_value(*max) })),
        AssertionCondition::NumericBetween { min, max } => Some(json!({
            "min": finite_number_value(*min),
            "max": finite_number_value(*max),
        })),
        AssertionCondition::AllowedHosts(hosts) => Some(json!({ "allowed_hosts": hosts })),
        AssertionCondition::JsonShape { required } => serde_json::to_value(json!({
            "required": required,
        }))
        .ok(),
    }
}

fn observed_summary(observed: &RuntimeValue) -> Value {
    let state = match observed.state {
        RuntimeState::Value => "value",
        RuntimeState::Missing => "missing",
        RuntimeState::Empty => "empty",
        RuntimeState::Error(state) => state.as_str(),
    };
    let mut object = Map::new();
    object.insert("state".to_owned(), Value::String(state.to_owned()));
    if let Some(value) = &observed.value {
        object.insert("value".to_owned(), bounded_value(value, 20));
    }
    if let Some(detail) = &observed.detail {
        object.insert(
            "detail".to_owned(),
            Value::String(bounded_line(detail, DEFAULT_EVIDENCE_CHARS)),
        );
    }
    Value::Object(object)
}

fn finding_target(target: &AssertionTarget) -> AssertionFindingTarget {
    match target {
        AssertionTarget::Field(field) => AssertionFindingTarget::Field {
            field: field.as_str().to_owned(),
        },
        AssertionTarget::Extractor(extractor) => AssertionFindingTarget::Extractor {
            extractor: extractor.clone(),
        },
    }
}

fn optional_string_runtime(value: Option<String>, evidence: Vec<String>) -> RuntimeValue {
    match value {
        None => RuntimeValue::with_state(RuntimeState::Missing, None, evidence, None),
        Some(value) if value.is_empty() => RuntimeValue::with_state(
            RuntimeState::Empty,
            Some(Value::String(value)),
            evidence,
            None,
        ),
        Some(value) => RuntimeValue::value(Value::String(value), evidence),
    }
}

fn string_list_runtime(values: &[String]) -> RuntimeValue {
    let value = Value::Array(values.iter().cloned().map(Value::String).collect());
    let state = if values.is_empty() || values.iter().all(String::is_empty) {
        RuntimeState::Empty
    } else {
        RuntimeState::Value
    };
    RuntimeValue::with_state(state, Some(value), values.to_vec(), None)
}

fn json_ld_block_value(block: &JsonLdBlock) -> Value {
    json!({
        "raw": block.raw,
        "valid_json": block.valid_json,
        "types": block.types,
    })
}

fn evidence_from_json_blocks(blocks: &[JsonLdBlock]) -> Vec<String> {
    blocks
        .iter()
        .take(DEFAULT_MAX_VALUES)
        .map(|block| bounded_line(&block.raw, DEFAULT_EVIDENCE_CHARS))
        .collect()
}

fn redirect_evidence(hops: &[RedirectHop]) -> Vec<String> {
    hops.iter()
        .take(DEFAULT_MAX_VALUES)
        .map(|hop| format!("{} {} {}", hop.from, hop.status, hop.to))
        .collect()
}

fn normalize_crawl_identity(raw: &str) -> String {
    Url::parse(raw)
        .ok()
        .map(|url| normalize_url(&url))
        .unwrap_or_else(|| raw.to_owned())
}

fn supports_html(content_type: Option<&str>) -> bool {
    match content_type {
        None => true,
        Some("text/html" | "application/xhtml+xml") => true,
        Some(_) => false,
    }
}

fn unsupported_html(item: &CrawlItem, source: ExtractionSource) -> ExtractionResult {
    ExtractionResult {
        state: ExtractionState::UnsupportedSource,
        source,
        value: None,
        evidence: Vec::new(),
        detail: Some(
            item.response
                .content_type
                .clone()
                .unwrap_or_else(|| "missing_content_type".to_owned()),
        ),
    }
}

fn limit_or_parse_error(source: ExtractionSource, detail: &str) -> ExtractionResult {
    parse_error(source, detail.to_owned())
}

fn parse_error(source: ExtractionSource, detail: impl Into<String>) -> ExtractionResult {
    ExtractionResult {
        state: ExtractionState::ParseError,
        source,
        value: None,
        evidence: Vec::new(),
        detail: Some(bounded_line(&detail.into(), DEFAULT_EVIDENCE_CHARS)),
    }
}

#[derive(Clone, Copy)]
enum EmptyBehavior {
    AllEmptyIsEmpty,
}

fn string_list_result(
    values: Vec<String>,
    matched_any: bool,
    source: ExtractionSource,
    extractor: &ResolvedExtractor,
    _empty_behavior: EmptyBehavior,
) -> ExtractionResult {
    if values.is_empty() && !matched_any {
        return state_result(ExtractionState::Missing, source, None, Vec::new(), None);
    }
    if values.len() > extractor.max_values {
        return limit_exceeded(source, "too many values");
    }
    if values
        .iter()
        .any(|value| value.chars().count() > extractor.max_scalar_chars)
    {
        return limit_exceeded(source, "a value is longer than max_scalar_chars");
    }
    let all_values_empty = values.iter().all(|value| value.is_empty());
    let evidence = values
        .iter()
        .take(DEFAULT_MAX_VALUES)
        .map(|value| bounded_line(value, extractor.evidence_chars))
        .collect::<Vec<_>>();
    let value = Value::Array(values.into_iter().map(Value::String).collect());
    let state = if all_values_empty {
        ExtractionState::Empty
    } else {
        ExtractionState::Value
    };
    value_result_with_state(value, source, evidence, extractor, state)
}

fn json_list_result(
    values: Vec<Value>,
    source: ExtractionSource,
    extractor: &ResolvedExtractor,
    require_object_nodes: bool,
) -> ExtractionResult {
    if values.is_empty() {
        return state_result(ExtractionState::Missing, source, None, Vec::new(), None);
    }
    if values.len() > extractor.max_values {
        return limit_exceeded(source, "too many values");
    }
    if require_object_nodes && values.iter().any(|value| !value.is_object()) {
        return parse_error(source, "the JSON-LD node is not an object");
    }
    for value in &values {
        let Ok(serialized) = serde_json::to_vec(value) else {
            return limit_exceeded(source, "the value could not be stored");
        };
        if serialized.len() > MAX_SERIALIZED_VALUE_BYTES {
            return limit_exceeded(source, "the values are larger than the byte limit");
        }
    }
    let evidence = values
        .iter()
        .take(DEFAULT_MAX_VALUES)
        .map(|value| bounded_line(&stable_json(value), extractor.evidence_chars))
        .collect::<Vec<_>>();
    value_result(Value::Array(values), source, evidence, extractor)
}

fn url_part_result(
    raw_url: &str,
    part: UrlPart,
    extractor: &ResolvedExtractor,
) -> ExtractionResult {
    let Ok(url) = Url::parse(raw_url) else {
        return state_result(
            ExtractionState::Missing,
            ExtractionSource::Url,
            None,
            Vec::new(),
            None,
        );
    };
    let value = match part {
        UrlPart::Scheme => Some(url.scheme().to_owned()),
        UrlPart::Host => url.host_str().map(str::to_owned),
        UrlPart::Path => Some(url.path().to_owned()),
        UrlPart::Query => url.query().map(|query| query.to_owned()),
        UrlPart::Extension => url
            .path_segments()
            .and_then(|mut segments| segments.next_back())
            .and_then(|segment| {
                segment
                    .rsplit_once('.')
                    .map(|(_, extension)| extension.to_owned())
            })
            .filter(|extension| !extension.is_empty()),
        UrlPart::NormalizedUrl => Some(normalize_url(&url)),
    };

    match value {
        None => state_result(
            ExtractionState::Missing,
            ExtractionSource::Url,
            None,
            Vec::new(),
            None,
        ),
        Some(value) if value.is_empty() => value_result_with_state(
            Value::String(value),
            ExtractionSource::Url,
            vec![String::new()],
            extractor,
            ExtractionState::Empty,
        ),
        Some(value) => {
            if value.chars().count() > extractor.max_scalar_chars {
                return limit_exceeded(
                    ExtractionSource::Url,
                    "a value is longer than max_scalar_chars",
                );
            }
            value_result(
                Value::String(value.clone()),
                ExtractionSource::Url,
                vec![bounded_line(&value, extractor.evidence_chars)],
                extractor,
            )
        }
    }
}

fn value_result(
    value: Value,
    source: ExtractionSource,
    evidence: Vec<String>,
    extractor: &ResolvedExtractor,
) -> ExtractionResult {
    value_result_with_state(value, source, evidence, extractor, ExtractionState::Value)
}

fn value_result_with_state(
    value: Value,
    source: ExtractionSource,
    evidence: Vec<String>,
    extractor: &ResolvedExtractor,
    state: ExtractionState,
) -> ExtractionResult {
    let serialized = serde_json::to_vec(&value).unwrap_or_default();
    if serialized.len() > MAX_SERIALIZED_VALUE_BYTES {
        return limit_exceeded(source, "the values are larger than the byte limit");
    }
    if scalar_too_long(&value, extractor.max_scalar_chars) {
        return limit_exceeded(source, "a value is longer than max_scalar_chars");
    }
    state_result(state, source, Some(value), evidence, None)
}

fn state_result(
    state: ExtractionState,
    source: ExtractionSource,
    value: Option<Value>,
    evidence: Vec<String>,
    detail: Option<String>,
) -> ExtractionResult {
    ExtractionResult {
        state,
        source,
        value,
        evidence,
        detail,
    }
}

fn limit_exceeded(source: ExtractionSource, detail: &str) -> ExtractionResult {
    state_result(
        ExtractionState::LimitExceeded,
        source,
        None,
        Vec::new(),
        Some(detail.to_owned()),
    )
}

fn scalar_too_long(value: &Value, max_chars: usize) -> bool {
    match value {
        Value::String(text) => text.chars().count() > max_chars,
        Value::Array(items) => items.iter().any(|item| scalar_too_long(item, max_chars)),
        Value::Object(object) => object.values().any(|item| scalar_too_long(item, max_chars)),
        _ => false,
    }
}

fn json_ld_nodes(document: &Html, schema_type: Option<&str>) -> Result<Vec<Value>, String> {
    let script_selector = Selector::parse("script[type]").expect("static script selector parses");
    let mut nodes = Vec::new();
    for script in document.select(&script_selector).filter(|script| {
        attr_media_type_eq_ignore_ascii_case(script, "type", "application/ld+json")
    }) {
        let raw = element_text(&script);
        let parsed = serde_json::from_str::<Value>(&raw)
            .map_err(|_| "the JSON-LD block is not valid JSON".to_owned())?;
        enumerate_json_ld_nodes(&parsed, &mut nodes);
    }
    if let Some(schema_type) = schema_type {
        nodes.retain(|node| json_ld_type_matches(node, schema_type));
    }
    Ok(nodes)
}

fn enumerate_json_ld_nodes(value: &Value, output: &mut Vec<Value>) {
    match value {
        Value::Object(object) => {
            output.push(value.clone());
            match object.get("@graph") {
                Some(Value::Array(items)) => output.extend(items.iter().cloned()),
                Some(Value::Object(_)) => output.push(object["@graph"].clone()),
                _ => {}
            }
        }
        Value::Array(items) => output.extend(items.iter().cloned()),
        _ => {}
    }
}

fn json_ld_type_matches(value: &Value, expected: &str) -> bool {
    let Some(found) = value.get("@type") else {
        return false;
    };
    match found {
        Value::String(value) => value == expected,
        Value::Array(items) => items.iter().any(|item| item.as_str() == Some(expected)),
        _ => false,
    }
}

fn document_text_without_code(document: &Html) -> String {
    let body = document.root_element().html();
    let body = strip_element_blocks(&body, "script");
    let body = strip_element_blocks(&body, "style");
    let parsed = Html::parse_document(&body);
    collapse_whitespace(&parsed.root_element().text().collect::<String>())
}

fn strip_element_blocks(input: &str, tag: &str) -> String {
    let pattern = format!(r"(?is)<{tag}\b[^>]*>.*?</{tag}\s*>");
    Regex::new(&pattern)
        .expect("static generated regex parses")
        .replace_all(input, "")
        .into_owned()
}

fn attr_media_type_eq_ignore_ascii_case(
    element: &ElementRef<'_>,
    name: &str,
    expected: &str,
) -> bool {
    element.value().attr(name).is_some_and(|value| {
        value
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .eq_ignore_ascii_case(expected)
    })
}

fn element_text(element: &ElementRef<'_>) -> String {
    element.text().collect::<String>()
}

fn collapse_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn bounded_line(input: &str, max_chars: usize) -> String {
    let sanitized = input.replace(['\n', '\r'], " ");
    if sanitized.chars().count() <= max_chars {
        return sanitized;
    }
    if max_chars <= 3 {
        return sanitized.chars().take(max_chars).collect();
    }
    let mut output = sanitized.chars().take(max_chars - 3).collect::<String>();
    output.push_str("...");
    output
}

fn stable_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "<unserializable>".to_owned())
}

fn bounded_value(value: &Value, max_items: usize) -> Value {
    match value {
        Value::String(text) => Value::String(bounded_line(text, DEFAULT_MAX_SCALAR_CHARS)),
        Value::Array(items) => {
            let mut output = items
                .iter()
                .take(max_items)
                .map(|item| bounded_value(item, max_items))
                .collect::<Vec<_>>();
            if items.len() > max_items {
                output.push(json!({ "omitted_count": items.len() - max_items }));
            }
            Value::Array(output)
        }
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| (key.clone(), bounded_value(value, max_items)))
                .collect(),
        ),
        _ => value.clone(),
    }
}

fn finite_number_value(value: f64) -> Value {
    Number::from_f64(value)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

pub fn validate_rule_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    if bytes.is_empty() || bytes.len() > 64 {
        return false;
    }
    if !bytes[0].is_ascii_lowercase() {
        return false;
    }
    bytes
        .iter()
        .all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
}

pub fn valid_json_pointer(pointer: &str) -> bool {
    if pointer.is_empty() {
        return true;
    }
    pointer.starts_with('/')
        && pointer.split('/').skip(1).all(|segment| {
            let mut chars = segment.chars();
            while let Some(ch) = chars.next() {
                if ch == '~' && !matches!(chars.next(), Some('0' | '1')) {
                    return false;
                }
            }
            true
        })
}

pub fn canonical_custom_extraction(
    mut custom: CustomExtraction,
) -> Result<CustomExtraction, AssertionError> {
    custom.catalog.sort_by(|left, right| left.id.cmp(&right.id));
    let known = custom
        .catalog
        .iter()
        .map(|extractor| extractor.id.as_str())
        .collect::<BTreeSet<_>>();
    for (url, per_url) in &custom.values {
        let total = serde_json::to_vec(per_url)
            .map(|bytes| bytes.len())
            .unwrap_or(0);
        if total > MAX_TOTAL_EXTRACTION_BYTES_PER_URL {
            return Err(AssertionError::InvalidRuntime(format!(
                "the extractor values for {url} are larger than the limit for one URL"
            )));
        }
        for id in per_url.keys() {
            if !known.contains(id.as_str()) {
                return Err(AssertionError::InvalidRuntime(format!(
                    "the stored values name an extractor {id:?} that the config does not have"
                )));
            }
        }
    }
    Ok(custom)
}

/// What `auditroo assert --dry-run` prints: which URLs of a snapshot each
/// extractor and assertion would cover.
pub fn coverage_summary(
    extractors: &[ResolvedExtractor],
    assertions: &[ResolvedAssertion],
    snapshot: Option<&Snapshot>,
) -> String {
    let extractor_map = extractors
        .iter()
        .cloned()
        .map(|extractor| (extractor.id.clone(), extractor))
        .collect::<BTreeMap<_, _>>();
    let mut out = format!(
        "{} extractor{} and {} assertion{} in the config.\n",
        extractors.len(),
        if extractors.len() == 1 { "" } else { "s" },
        assertions.len(),
        if assertions.len() == 1 { "" } else { "s" }
    );
    let Some(snapshot) = snapshot else {
        out.push_str("Pass --snapshot <file> to see which crawled URLs they match.\n");
        for extractor in extractors {
            out.push_str(&format!(
                "  extractor {} ({})\n",
                extractor.id,
                extractor.kind().as_str()
            ));
        }
        for assertion in assertions {
            out.push_str(&format!(
                "  assertion {} ({})\n",
                assertion.id,
                assertion.operator.as_str()
            ));
        }
        return out;
    };

    let pages: Vec<&String> = snapshot.records.keys().collect();
    let stored = snapshot.custom_extraction.as_ref();
    let mut stale = false;
    if !extractors.is_empty() {
        out.push_str("\nExtractors\n");
    }
    for extractor in extractors {
        let matched = pages
            .iter()
            .filter(|url| extractor.scope.matches_url(url))
            .count();
        let same_definition = stored.is_some_and(|custom| {
            custom
                .catalog
                .iter()
                .any(|stored| stored.id == extractor.id && stored == extractor)
        });
        let with_values = stored.map_or(0, |custom| {
            custom
                .values
                .iter()
                .filter(|(url, values)| {
                    extractor.scope.matches_url(url) && values.contains_key(&extractor.id)
                })
                .count()
        });
        let values = if same_definition {
            format!("values stored for {with_values} of them")
        } else {
            stale = true;
            "the snapshot has no values for this version of the extractor".to_owned()
        };
        out.push_str(&format!(
            "  {} ({}): matches {} of {} URLs; {values}\n",
            extractor.id,
            extractor.kind().as_str(),
            matched,
            pages.len()
        ));
    }
    if !assertions.is_empty() {
        out.push_str("\nAssertions\n");
    }
    let mut unmatched = Vec::new();
    for assertion in assertions {
        let matched = pages
            .iter()
            .filter(|url| assertion.target_matches_url(url, &extractor_map))
            .count();
        if matched == 0 {
            unmatched.push(assertion.id.clone());
        }
        let target = match &assertion.target {
            AssertionTarget::Field(field) => format!("field {}", field.as_str()),
            AssertionTarget::Extractor(id) => format!("extractor {id}"),
        };
        out.push_str(&format!(
            "  {} ({} on {target}): checks {} of {} URLs\n",
            assertion.id,
            assertion.operator.as_str(),
            matched,
            pages.len()
        ));
    }
    if stale {
        out.push_str(
            "\nTake a new snapshot with this config so it stores the extractor values: auditroo snapshot <url> --out <file> --config <this config>\n",
        );
    }
    if !unmatched.is_empty() {
        out.push_str(&format!(
            "\nThese assertions match no URL in the snapshot, so a crawl with this config would stop with an error: {}\n",
            unmatched.join(", ")
        ));
    }
    out
}
