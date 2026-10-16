//! The bundle's `parallel.yaml`: which processors may run, what each costs,
//! how long a workflow caller waits for one, and the monthly cap.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

pub const FILE: &str = "parallel.yaml";

#[derive(Debug, Clone, Deserialize)]
pub struct Processor {
    pub usd: f64,
    pub wait_minutes: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub monthly_cap_usd: f64,
    pub default_processor: String,
    pub processors: BTreeMap<String, Processor>,
}

impl Config {
    pub fn parse(text: &str) -> Result<Self> {
        let c: Config = serde_yaml::from_str(text)?;
        if c.processors.is_empty() {
            bail!("no processors listed");
        }
        if !c.processors.contains_key(&c.default_processor) {
            bail!(
                "default_processor `{}` is not among the processors",
                c.default_processor
            );
        }
        if c.monthly_cap_usd.is_nan() || c.monthly_cap_usd < 0.0 {
            bail!("monthly_cap_usd must be zero or more");
        }
        if let Some((name, _)) = c
            .processors
            .iter()
            .find(|(_, p)| p.usd.is_nan() || p.usd <= 0.0)
        {
            bail!("processor `{name}` needs a price above zero");
        }
        Ok(c)
    }

    pub fn load_from(dir: &Path) -> Result<Self> {
        let p = dir.join(FILE);
        let text =
            std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
        Self::parse(&text).with_context(|| format!("parsing {}", p.display()))
    }

    pub fn names(&self) -> Vec<&str> {
        self.processors.keys().map(String::as_str).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_file_parses() {
        let dir = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/.."));
        let c = Config::load_from(dir).expect("parallel.yaml parses");
        assert!(c.processors.contains_key("pro"));
        assert!(c.monthly_cap_usd > 0.0);
    }

    #[test]
    fn a_default_that_is_not_listed_is_refused() {
        let e = Config::parse(
            "monthly_cap_usd: 5\ndefault_processor: x\nprocessors: {pro: {usd: 0.1, wait_minutes: 30}}",
        )
        .unwrap_err();
        assert!(e.to_string().contains("default_processor"));
    }

    #[test]
    fn a_free_processor_is_refused() {
        // A zero price would let runs past the cap uncounted.
        let e = Config::parse(
            "monthly_cap_usd: 5\ndefault_processor: pro\nprocessors: {pro: {usd: 0, wait_minutes: 30}}",
        )
        .unwrap_err();
        assert!(e.to_string().contains("price"));
    }
}
