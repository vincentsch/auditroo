//! Progress lines on stderr while a crawl runs.
//!
//! In a terminal one line is redrawn in place. In CI logs and pipes there is
//! one line when a crawl starts and one when it ends. `--quiet` turns both off.

use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

pub struct Progress {
    enabled: bool,
    live: bool,
    label: String,
    started: Instant,
    last_draw: Option<Instant>,
}

impl Progress {
    pub fn new(quiet: bool) -> Self {
        let enabled = !quiet;
        Self {
            enabled,
            live: enabled && std::io::stderr().is_terminal(),
            label: String::new(),
            started: Instant::now(),
            last_draw: None,
        }
    }

    pub fn quiet() -> Self {
        Self::new(true)
    }

    /// A line of its own, for example a note before the crawl starts.
    pub fn message(&self, text: &str) {
        if self.enabled {
            eprintln!("{text}");
        }
    }

    pub fn start(&mut self, label: &str, url: &str) {
        self.label = format!("{label} {url}");
        self.started = Instant::now();
        self.last_draw = None;
        if self.enabled && !self.live {
            eprintln!("{} ...", self.label);
        }
    }

    pub fn update(&mut self, pages: usize, failed: usize) {
        if !self.live {
            return;
        }
        let now = Instant::now();
        if self
            .last_draw
            .is_some_and(|last| now.duration_since(last) < Duration::from_millis(100))
        {
            return;
        }
        self.last_draw = Some(now);
        let mut stderr = std::io::stderr();
        let _ = write!(
            stderr,
            "\r\x1b[2K{}: {} URLs{} ({:.0} s)",
            self.label,
            pages,
            failed_text(failed),
            self.started.elapsed().as_secs_f64()
        );
        let _ = stderr.flush();
    }

    pub fn finish(&mut self, pages: usize, failed: usize) -> Duration {
        let elapsed = self.started.elapsed();
        if self.enabled {
            if self.live {
                eprint!("\r\x1b[2K");
            }
            eprintln!(
                "{}: {} URLs{} in {:.1} s",
                self.label,
                pages,
                failed_text(failed),
                elapsed.as_secs_f64()
            );
        }
        elapsed
    }
}

fn failed_text(failed: usize) -> String {
    match failed {
        0 => String::new(),
        1 => ", 1 could not be loaded".to_owned(),
        count => format!(", {count} could not be loaded"),
    }
}
