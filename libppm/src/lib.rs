//! Package management library.

mod search;

use std::fs;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const CATALOG_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../catalog.json");

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct App {
    pub id: u64,
    pub name: String,
    pub description: String,
    pub package_source: PackageSource,
    pub traffic: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PackageSource {
    pub manager: String,
    pub package: String,
}

pub struct PackageManager {
    search: search::Search,
}

impl PackageManager {
    pub fn init() -> Result<Self> {
        let catalog = fs::read(CATALOG_PATH).with_context(|| format!("reading {CATALOG_PATH}"))?;
        let apps: Vec<App> = serde_json::from_slice(&catalog).context("parsing catalog.json")?;

        Ok(Self {
            search: search::Search::build(&apps)?,
        })
    }

    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<App>> {
        self.search.query(query, limit)
    }
}
