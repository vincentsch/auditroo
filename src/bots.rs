//! The bot catalog says which User-Agent text belongs to which crawler.
//! `[[logs.bot_rule]]` entries in auditroo.toml add rules or replace
//! built-in ones.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::assertions::validate_rule_id;

pub const BOT_CATALOG_SCHEMA_VERSION: u16 = 1;
pub const BUILTIN_BOT_CATALOG_VERSION: &str = "builtin-2026-10";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientType {
    Search,
    AiBot,
    SocialPreview,
    UptimeMonitoring,
    GenericAutomated,
    BrowserLike,
    Unknown,
}

impl ClientType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Search => "search",
            Self::AiBot => "ai_bot",
            Self::SocialPreview => "social_preview",
            Self::UptimeMonitoring => "uptime_monitoring",
            Self::GenericAutomated => "generic_automated",
            Self::BrowserLike => "browser_like",
            Self::Unknown => "unknown",
        }
    }

    pub fn is_classified_bot(self) -> bool {
        matches!(
            self,
            Self::Search
                | Self::AiBot
                | Self::SocialPreview
                | Self::UptimeMonitoring
                | Self::GenericAutomated
        )
    }
}

impl fmt::Display for ClientType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::str::FromStr for ClientType {
    type Err = String;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        match input {
            "search" => Ok(Self::Search),
            "ai_bot" => Ok(Self::AiBot),
            "social_preview" => Ok(Self::SocialPreview),
            "uptime_monitoring" => Ok(Self::UptimeMonitoring),
            "generic_automated" => Ok(Self::GenericAutomated),
            "browser_like" => Ok(Self::BrowserLike),
            "unknown" => Ok(Self::Unknown),
            _ => Err(format!(
                "unknown client_type {input:?}. Use search, ai_bot, social_preview, uptime_monitoring, generic_automated, browser_like or unknown"
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BotRuleSource {
    Builtin,
    Config,
}

impl BotRuleSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Config => "config",
        }
    }
}

impl fmt::Display for BotRuleSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BotRule {
    pub precedence: u32,
    pub id: String,
    pub family_id: String,
    pub display_name: String,
    pub client_type: ClientType,
    pub pattern: String,
    pub source: BotRuleSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BotRuleConfig {
    pub id: String,
    pub family_id: String,
    pub display_name: String,
    pub client_type: ClientType,
    pub pattern: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BotCatalog {
    pub schema_version: u16,
    pub catalog_version: String,
    pub effective_fingerprint: String,
    pub rules: Vec<BotRule>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BotClassification {
    pub matched_rule_id: Option<String>,
    pub bot_family: Option<String>,
    pub client_type: ClientType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BotCatalogError {
    InvalidRule(String),
    DuplicateRule(String),
}

impl fmt::Display for BotCatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRule(detail) => {
                write!(
                    formatter,
                    "a [[logs.bot_rule]] entry is not valid: {detail}"
                )
            }
            Self::DuplicateRule(id) => {
                write!(formatter, "two [[logs.bot_rule]] entries use the id {id:?}")
            }
        }
    }
}

impl Error for BotCatalogError {}

impl BotCatalog {
    pub fn builtin() -> Self {
        Self::with_config_rules(Vec::new()).expect("built-in catalog validates")
    }

    pub fn with_config_rules(rules: Vec<BotRuleConfig>) -> Result<Self, BotCatalogError> {
        let mut effective = builtin_rules();
        let mut inserted = Vec::new();
        let mut seen_user_ids = BTreeSet::new();

        // A rule from the config with a built-in id replaces that rule. New
        // rules come after the specific built-in rules and before the
        // generic ones such as "bot" (precedence 900 and up).
        for (index, rule) in rules.into_iter().enumerate() {
            validate_config_rule(&rule)?;
            if !seen_user_ids.insert(rule.id.clone()) {
                return Err(BotCatalogError::DuplicateRule(rule.id));
            }

            if let Some(existing) = effective.iter_mut().find(|existing| existing.id == rule.id) {
                *existing = BotRule {
                    precedence: existing.precedence,
                    id: rule.id,
                    family_id: rule.family_id,
                    display_name: rule.display_name,
                    client_type: rule.client_type,
                    pattern: rule.pattern,
                    source: BotRuleSource::Config,
                };
            } else {
                inserted.push(BotRule {
                    precedence: 800 + index as u32,
                    id: rule.id,
                    family_id: rule.family_id,
                    display_name: rule.display_name,
                    client_type: rule.client_type,
                    pattern: rule.pattern,
                    source: BotRuleSource::Config,
                });
            }
        }

        let mut provider = effective
            .iter()
            .filter(|rule| rule.precedence < 900)
            .cloned()
            .collect::<Vec<_>>();
        let mut generic = effective
            .into_iter()
            .filter(|rule| rule.precedence >= 900)
            .collect::<Vec<_>>();
        provider.sort_by_key(|rule| (rule.precedence, rule.id.clone()));
        generic.sort_by_key(|rule| (rule.precedence, rule.id.clone()));

        let mut rules = provider;
        rules.extend(inserted);
        rules.extend(generic);
        let effective_fingerprint = fingerprint_rules(&rules);
        Ok(Self {
            schema_version: BOT_CATALOG_SCHEMA_VERSION,
            catalog_version: BUILTIN_BOT_CATALOG_VERSION.to_owned(),
            effective_fingerprint,
            rules,
        })
    }

    pub fn classify(&self, user_agent: Option<&str>) -> BotClassification {
        let Some(user_agent) = user_agent else {
            return unknown_classification();
        };
        if user_agent.is_empty() {
            return unknown_classification();
        }

        let user_agent = user_agent.to_ascii_lowercase();
        for rule in &self.rules {
            if user_agent.contains(&rule.pattern.to_ascii_lowercase()) {
                return BotClassification {
                    matched_rule_id: Some(rule.id.clone()),
                    bot_family: Some(rule.family_id.clone()),
                    client_type: rule.client_type,
                };
            }
        }

        unknown_classification()
    }
}

fn unknown_classification() -> BotClassification {
    BotClassification {
        matched_rule_id: None,
        bot_family: None,
        client_type: ClientType::Unknown,
    }
}

fn validate_config_rule(rule: &BotRuleConfig) -> Result<(), BotCatalogError> {
    validate_rule_identity(&rule.id, "id")?;
    validate_rule_identity(&rule.family_id, "family_id")?;
    if rule.display_name.is_empty()
        || rule.display_name.chars().count() > 80
        || rule.display_name.chars().any(char::is_control)
    {
        return Err(BotCatalogError::InvalidRule(
            "display_name must be 1 to 80 characters, without control characters".to_owned(),
        ));
    }
    if rule.pattern.is_empty() || rule.pattern.len() > 256 || !rule.pattern.is_ascii() {
        return Err(BotCatalogError::InvalidRule(
            "pattern must be ASCII text of 1 to 256 bytes".to_owned(),
        ));
    }
    if rule.client_type == ClientType::Unknown {
        return Err(BotCatalogError::InvalidRule(
            "client_type = \"unknown\" is not allowed in your own bot rules".to_owned(),
        ));
    }
    Ok(())
}

fn validate_rule_identity(id: &str, label: &str) -> Result<(), BotCatalogError> {
    let bytes = id.as_bytes();
    if validate_rule_id(id) && !bytes.iter().any(|byte| matches!(byte, b'.' | b'_')) {
        return Ok(());
    }
    Err(BotCatalogError::InvalidRule(format!(
        "{label} must match ^[a-z][a-z0-9-]{{0,63}}$"
    )))
}

/// The built-in catalog. Rules are tried in precedence order and match when
/// the user agent contains the pattern, ignoring case. Specific rules come
/// before the generic "bot", "crawler" and "spider" rules.
fn builtin_rules() -> Vec<BotRule> {
    use ClientType::{
        AiBot, BrowserLike, GenericAutomated, Search, SocialPreview, UptimeMonitoring,
    };
    [
        // Search engines.
        (
            10,
            "googlebot",
            "googlebot",
            "Googlebot",
            Search,
            "Googlebot",
        ),
        (
            11,
            "google-inspectiontool",
            "googlebot",
            "Google Search Console",
            Search,
            "Google-InspectionTool",
        ),
        (20, "bingbot", "bingbot", "Bingbot", Search, "bingbot/"),
        (
            30,
            "applebot",
            "applebot",
            "Applebot (Siri and Spotlight)",
            Search,
            "Applebot/",
        ),
        (
            40,
            "duckduckbot",
            "duckduckbot",
            "DuckDuckBot",
            Search,
            "DuckDuckBot",
        ),
        (
            50,
            "yandexbot",
            "yandexbot",
            "YandexBot",
            Search,
            "YandexBot",
        ),
        (
            60,
            "baiduspider",
            "baiduspider",
            "Baiduspider",
            Search,
            "Baiduspider",
        ),
        (
            70,
            "petalbot",
            "petalbot",
            "PetalBot (Huawei)",
            Search,
            "PetalBot",
        ),
        // AI assistants and AI crawlers.
        (
            100,
            "oai-searchbot",
            "oai-searchbot",
            "OAI-SearchBot (ChatGPT search)",
            AiBot,
            "OAI-SearchBot",
        ),
        (
            101,
            "chatgpt-user",
            "chatgpt-user",
            "ChatGPT-User (fetches for a ChatGPT user)",
            AiBot,
            "ChatGPT-User",
        ),
        (
            102,
            "gptbot",
            "gptbot",
            "GPTBot (OpenAI training)",
            AiBot,
            "GPTBot",
        ),
        (
            110,
            "claude-searchbot",
            "claude-searchbot",
            "Claude-SearchBot (Claude search)",
            AiBot,
            "Claude-SearchBot",
        ),
        (
            111,
            "claude-user",
            "claude-user",
            "Claude-User (fetches for a Claude user)",
            AiBot,
            "Claude-User",
        ),
        (
            112,
            "claude-bot",
            "claude-bot",
            "ClaudeBot (Anthropic training)",
            AiBot,
            "ClaudeBot",
        ),
        (
            120,
            "perplexitybot",
            "perplexitybot",
            "PerplexityBot (Perplexity search)",
            AiBot,
            "PerplexityBot",
        ),
        (
            121,
            "perplexity-user",
            "perplexity-user",
            "Perplexity-User (fetches for a Perplexity user)",
            AiBot,
            "Perplexity-User",
        ),
        (
            130,
            "duckassistbot",
            "duckassistbot",
            "DuckAssistBot (DuckDuckGo answers)",
            AiBot,
            "DuckAssistBot",
        ),
        (
            131,
            "mistralai-user",
            "mistralai-user",
            "MistralAI-User (fetches for a Le Chat user)",
            AiBot,
            "MistralAI-User",
        ),
        (
            140,
            "meta-externalagent",
            "meta-externalagent",
            "meta-externalagent (Meta AI)",
            AiBot,
            "meta-externalagent",
        ),
        (
            141,
            "meta-externalfetcher",
            "meta-externalfetcher",
            "meta-externalfetcher (Meta AI user fetches)",
            AiBot,
            "meta-externalfetcher",
        ),
        (
            150,
            "amazonbot",
            "amazonbot",
            "Amazonbot",
            AiBot,
            "Amazonbot",
        ),
        (
            160,
            "bytespider",
            "bytespider",
            "Bytespider (ByteDance)",
            AiBot,
            "Bytespider",
        ),
        (
            170,
            "ccbot",
            "ccbot",
            "CCBot (Common Crawl)",
            AiBot,
            "CCBot",
        ),
        (
            180,
            "cohere-ai",
            "cohere-ai",
            "cohere-ai",
            AiBot,
            "cohere-ai",
        ),
        // Link previews in chat and social apps.
        (
            300,
            "facebookexternalhit",
            "facebookexternalhit",
            "Facebook link preview",
            SocialPreview,
            "facebookexternalhit",
        ),
        (
            301,
            "twitterbot",
            "twitterbot",
            "X (Twitter) link preview",
            SocialPreview,
            "Twitterbot",
        ),
        (
            302,
            "linkedinbot",
            "linkedinbot",
            "LinkedIn link preview",
            SocialPreview,
            "LinkedInBot",
        ),
        (
            303,
            "slackbot",
            "slackbot",
            "Slack link preview",
            SocialPreview,
            "Slackbot",
        ),
        (
            304,
            "discordbot",
            "discordbot",
            "Discord link preview",
            SocialPreview,
            "Discordbot",
        ),
        (
            305,
            "telegrambot",
            "telegrambot",
            "Telegram link preview",
            SocialPreview,
            "TelegramBot",
        ),
        (
            306,
            "whatsapp",
            "whatsapp",
            "WhatsApp link preview",
            SocialPreview,
            "WhatsApp/",
        ),
        (
            307,
            "pinterestbot",
            "pinterestbot",
            "Pinterest",
            SocialPreview,
            "Pinterestbot",
        ),
        // Uptime monitors.
        (
            400,
            "uptimerobot",
            "uptimerobot",
            "UptimeRobot",
            UptimeMonitoring,
            "UptimeRobot/",
        ),
        (
            401,
            "pingdom",
            "pingdom",
            "Pingdom",
            UptimeMonitoring,
            "Pingdom",
        ),
        (
            402,
            "betteruptime",
            "betteruptime",
            "Better Stack Uptime",
            UptimeMonitoring,
            "Better Uptime",
        ),
        // Anything else that says it is automated, then browsers.
        (
            900,
            "generic-bot",
            "generic-automated",
            "Other bots",
            GenericAutomated,
            "bot",
        ),
        (
            910,
            "generic-crawler",
            "generic-automated",
            "Other bots",
            GenericAutomated,
            "crawler",
        ),
        (
            920,
            "generic-spider",
            "generic-automated",
            "Other bots",
            GenericAutomated,
            "spider",
        ),
        (
            1000,
            "mozilla-browser",
            "browser-like",
            "Browsers",
            BrowserLike,
            "Mozilla/",
        ),
    ]
    .into_iter()
    .map(
        |(precedence, id, family_id, display_name, client_type, pattern)| BotRule {
            precedence,
            id: id.to_owned(),
            family_id: family_id.to_owned(),
            display_name: display_name.to_owned(),
            client_type,
            pattern: pattern.to_owned(),
            source: BotRuleSource::Builtin,
        },
    )
    .collect()
}

/// The catalog as a short list for people.
pub fn render_catalog_human(catalog: &BotCatalog) -> String {
    let mut out = format!(
        "auditroo recognizes these clients in access logs ({} rules). A User-Agent matches the first rule whose text it contains.\n",
        catalog.rules.len()
    );
    let sections = [
        (ClientType::Search, "Search engines"),
        (ClientType::AiBot, "AI assistants and AI crawlers"),
        (ClientType::SocialPreview, "Link previews"),
        (ClientType::UptimeMonitoring, "Uptime monitors"),
        (ClientType::GenericAutomated, "Other automated clients"),
        (ClientType::BrowserLike, "Browsers"),
    ];
    for (client_type, title) in sections {
        let rules: Vec<&BotRule> = catalog
            .rules
            .iter()
            .filter(|rule| rule.client_type == client_type)
            .collect();
        if rules.is_empty() {
            continue;
        }
        out.push_str(&format!("\n{title}\n"));
        for rule in rules {
            let source = match rule.source {
                BotRuleSource::Builtin => "",
                BotRuleSource::Config => "  (from auditroo.toml)",
            };
            out.push_str(&format!(
                "  {:<48} \"{}\"{source}\n",
                rule.display_name, rule.pattern
            ));
        }
    }
    out
}

fn fingerprint_rules(rules: &[BotRule]) -> String {
    let mut canonical = String::new();
    canonical.push('[');
    for (index, rule) in rules.iter().enumerate() {
        if index > 0 {
            canonical.push(',');
        }
        canonical.push('{');
        push_json_field(&mut canonical, "client_type", rule.client_type.as_str());
        canonical.push(',');
        push_json_field(&mut canonical, "display_name", &rule.display_name);
        canonical.push(',');
        push_json_field(&mut canonical, "family_id", &rule.family_id);
        canonical.push(',');
        push_json_field(&mut canonical, "id", &rule.id);
        canonical.push(',');
        push_json_field(&mut canonical, "pattern", &rule.pattern);
        canonical.push_str(",\"precedence\":");
        canonical.push_str(&rule.precedence.to_string());
        canonical.push(',');
        push_json_field(&mut canonical, "source", rule.source.as_str());
        canonical.push('}');
    }
    canonical.push(']');

    let digest = Sha256::digest(canonical.as_bytes());
    let mut hex = String::with_capacity(71);
    hex.push_str("sha256:");
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

fn push_json_field(out: &mut String, key: &str, value: &str) {
    out.push('"');
    out.push_str(key);
    out.push_str("\":");
    out.push_str(&serde_json::to_string(value).expect("string serialization cannot fail"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_classify_specific_rules_before_generic_rules() {
        let catalog = BotCatalog::builtin();
        let google = catalog.classify(Some("Mozilla/5.0 Googlebot/2.1"));
        assert_eq!(google.client_type, ClientType::Search);
        assert_eq!(google.bot_family.as_deref(), Some("googlebot"));

        let gpt = catalog.classify(Some("GPTBot/1.0"));
        assert_eq!(gpt.client_type, ClientType::AiBot);
        assert_eq!(gpt.matched_rule_id.as_deref(), Some("gptbot"));

        let browser = catalog.classify(Some("Mozilla/5.0 Safari/605.1.15"));
        assert_eq!(browser.client_type, ClientType::BrowserLike);

        let unknown = catalog.classify(None);
        assert_eq!(unknown.client_type, ClientType::Unknown);
    }

    #[test]
    fn config_rules_override_and_extend_the_effective_catalog() {
        let builtin = BotCatalog::builtin();
        let catalog = BotCatalog::with_config_rules(vec![
            BotRuleConfig {
                id: "googlebot".to_owned(),
                family_id: "googlebot-news".to_owned(),
                display_name: "Googlebot News".to_owned(),
                client_type: ClientType::Search,
                pattern: "Googlebot-News".to_owned(),
            },
            BotRuleConfig {
                id: "example-crawler".to_owned(),
                family_id: "example-crawler".to_owned(),
                display_name: "ExampleCrawler".to_owned(),
                client_type: ClientType::GenericAutomated,
                pattern: "ExampleCrawler/".to_owned(),
            },
        ])
        .expect("catalog builds");

        assert_ne!(catalog.effective_fingerprint, builtin.effective_fingerprint);
        assert_eq!(
            catalog
                .classify(Some("ExampleCrawler/1.0"))
                .bot_family
                .as_deref(),
            Some("example-crawler")
        );
        assert_eq!(
            catalog
                .classify(Some("Googlebot-News"))
                .bot_family
                .as_deref(),
            Some("googlebot-news")
        );
    }
}
