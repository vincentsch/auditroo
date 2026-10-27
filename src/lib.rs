//! auditroo is a command-line tool that runs SEO regression tests for
//! websites.
//!
//! The library only exists so the integration tests and the binary can share
//! code. Its modules are not a stable API and can change in any release.

#[doc(hidden)]
pub mod adapters;
#[doc(hidden)]
pub mod analysis;
#[doc(hidden)]
pub mod assertions;
#[doc(hidden)]
pub mod audit;
#[doc(hidden)]
pub mod bots;
#[doc(hidden)]
pub mod bundle;
#[doc(hidden)]
pub mod cli;
#[doc(hidden)]
pub mod config;
#[doc(hidden)]
pub mod crawl;
#[doc(hidden)]
pub mod demo;
#[doc(hidden)]
pub mod error;
#[doc(hidden)]
pub mod external;
#[doc(hidden)]
pub mod extract;
#[doc(hidden)]
pub mod findings;
#[doc(hidden)]
pub mod graph;
#[doc(hidden)]
pub mod history;
#[doc(hidden)]
pub mod logs;
#[doc(hidden)]
pub mod monitor;
#[doc(hidden)]
pub mod notify;
#[doc(hidden)]
pub mod progress;
#[doc(hidden)]
pub mod recipes;
#[doc(hidden)]
pub mod render;
#[doc(hidden)]
pub mod report;
#[doc(hidden)]
pub mod rules;
#[doc(hidden)]
pub mod site;
#[doc(hidden)]
pub mod sitemap;
#[doc(hidden)]
pub mod snapshot;
