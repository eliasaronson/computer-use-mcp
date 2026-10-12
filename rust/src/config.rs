//! config.toml: server settings and the blocked/private window policy.

use std::path::Path;

use anyhow::{Context, Result};
use regex::{Regex, RegexBuilder};
use serde::Deserialize;

#[derive(Deserialize)]
struct File {
    server: Server,
    screenshot: Screenshot,
    blocked: Patterns,
    private: Patterns,
}

#[derive(Deserialize)]
pub struct Server {
    pub port: u16,
    #[serde(default = "default_lease")]
    pub lease_seconds: u64,
}

fn default_lease() -> u64 {
    90
}

#[derive(Deserialize)]
pub struct Screenshot {
    pub max_width: u32,
    pub max_height: u32,
}

#[derive(Deserialize, Default)]
struct Patterns {
    #[serde(default)]
    app_ids: Vec<String>,
    #[serde(default)]
    titles: Vec<String>,
}

struct Matcher {
    app_ids: Vec<Regex>,
    titles: Vec<Regex>,
}

impl Matcher {
    fn new(p: &Patterns) -> Result<Matcher> {
        let compile = |v: &Vec<String>| -> Result<Vec<Regex>> {
            v.iter()
                .map(|s| RegexBuilder::new(s).case_insensitive(true).build().with_context(|| format!("bad pattern {s:?}")))
                .collect()
        };
        Ok(Matcher { app_ids: compile(&p.app_ids)?, titles: compile(&p.titles)? })
    }

    fn matches(&self, app_id: &str, title: &str) -> bool {
        self.app_ids.iter().any(|r| r.is_match(app_id)) || self.titles.iter().any(|r| r.is_match(title))
    }
}

pub struct Policy {
    blocked: Matcher,
    private: Matcher,
}

impl Policy {
    pub fn blocked(&self, app_id: &str, title: &str) -> bool {
        self.blocked.matches(app_id, title)
    }

    pub fn private(&self, app_id: &str, title: &str) -> bool {
        self.private.matches(app_id, title)
    }
}

pub struct Config {
    pub server: Server,
    pub screenshot: Screenshot,
    pub policy: Policy,
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let f: File = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        Ok(Config {
            server: f.server,
            screenshot: f.screenshot,
            policy: Policy { blocked: Matcher::new(&f.blocked)?, private: Matcher::new(&f.private)? },
        })
    }
}
