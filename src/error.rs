//! Command errors and their exit codes.
//!
//! Exit code 1 means the gate failed: auditroo worked and found a problem.
//! Errors split into two kinds so CI can tell them apart:
//!
//! - 2: usage or configuration. Retrying will not help; fix the command,
//!   the config file or the input file.
//! - 3: network or runtime. The site was unreachable, a write failed, or a
//!   helper program crashed. A retry may work.

use std::error::Error;
use std::fmt;

pub const EXIT_OK: i32 = 0;
pub const EXIT_GATE_FAILED: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_RUNTIME: i32 = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    Usage(String),
    Runtime(String),
}

impl CliError {
    pub fn usage(message: impl fmt::Display) -> Self {
        Self::Usage(message.to_string())
    }

    pub fn runtime(message: impl fmt::Display) -> Self {
        Self::Runtime(message.to_string())
    }

    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Usage(_) => EXIT_USAGE,
            Self::Runtime(_) => EXIT_RUNTIME,
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage(message) | Self::Runtime(message) => formatter.write_str(message),
        }
    }
}

impl Error for CliError {}

macro_rules! usage_errors {
    ($($error:ty),* $(,)?) => {
        $(impl From<$error> for CliError {
            fn from(error: $error) -> Self {
                Self::Usage(error.to_string())
            }
        })*
    };
}

macro_rules! runtime_errors {
    ($($error:ty),* $(,)?) => {
        $(impl From<$error> for CliError {
            fn from(error: $error) -> Self {
                Self::Runtime(error.to_string())
            }
        })*
    };
}

usage_errors!(
    crate::config::ConfigError,
    crate::snapshot::SnapshotError,
    crate::report::ReportError,
    crate::assertions::AssertionError,
    crate::external::ImpactError,
    crate::recipes::RecipeError,
    crate::analysis::LogAnalysisError,
);

runtime_errors!(crate::adapters::QualityError, std::io::Error);

impl From<crate::monitor::MonitorError> for CliError {
    fn from(error: crate::monitor::MonitorError) -> Self {
        use crate::monitor::MonitorError;
        match error {
            MonitorError::Config(_) | MonitorError::InvalidBaseline(_) => {
                Self::Usage(error.to_string())
            }
            other => Self::Runtime(other.to_string()),
        }
    }
}

impl From<crate::crawl::FetchError> for CliError {
    fn from(error: crate::crawl::FetchError) -> Self {
        use crate::crawl::FetchError;
        match error {
            FetchError::InvalidHeader { .. }
            | FetchError::UserAgentHeaderNotAllowed { .. }
            | FetchError::InvalidUserAgent { .. } => Self::Usage(error.to_string()),
            other => Self::Runtime(other.to_string()),
        }
    }
}

impl From<crate::crawl::CrawlError> for CliError {
    fn from(error: crate::crawl::CrawlError) -> Self {
        use crate::crawl::CrawlError;
        match error {
            CrawlError::Configuration { .. } => Self::Usage(error.to_string()),
            other => Self::Runtime(other.to_string()),
        }
    }
}
