//! `<pool_dir>/config.toml` 运行时配置。缺失或缺字段时回落到 lib.rs 常量。

use crate::{
    DEFAULT_COOLDOWN_REGEX, DEFAULT_ERROR_COOLDOWN_MIN, DEFAULT_FLOCK_TIMEOUT_MS, DEFAULT_LOG_KEEP,
    ZOMBIE_MINUTES,
};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct Config {
    pub zombie_minutes: i64,
    pub default_error_cooldown_min: i64,
    pub cooldown_regex: String,
    pub log_keep: usize,
    pub flock_timeout_ms: u64,
    /// tier → model 注入表。wrap pick 到 profile 时，若用户未显式 --model，
    /// 则按 picked.kind 查表插入。缺表 / 缺键时不注入（让 kiro-cli 走 settings 默认）。
    pub tier_model: BTreeMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            zombie_minutes: ZOMBIE_MINUTES,
            default_error_cooldown_min: DEFAULT_ERROR_COOLDOWN_MIN,
            cooldown_regex: DEFAULT_COOLDOWN_REGEX.to_string(),
            log_keep: DEFAULT_LOG_KEEP,
            flock_timeout_ms: DEFAULT_FLOCK_TIMEOUT_MS,
            tier_model: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Deserialize, Default)]
struct Raw {
    #[serde(default)]
    zombie_minutes: Option<i64>,
    #[serde(default)]
    default_error_cooldown_min: Option<i64>,
    #[serde(default)]
    cooldown_regex: Option<String>,
    #[serde(default)]
    log_keep: Option<usize>,
    #[serde(default)]
    flock_timeout_ms: Option<u64>,
    #[serde(default)]
    tier_model: Option<BTreeMap<String, String>>,
}

impl Config {
    pub fn load(pool_dir: &Path) -> Result<Self> {
        let p = pool_dir.join("config.toml");
        if !p.exists() {
            return Ok(Self::default());
        }
        let body = std::fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
        let raw: Raw = toml::from_str(&body).with_context(|| format!("parse {}", p.display()))?;
        let d = Self::default();
        Ok(Self {
            zombie_minutes: raw.zombie_minutes.unwrap_or(d.zombie_minutes),
            default_error_cooldown_min: raw
                .default_error_cooldown_min
                .unwrap_or(d.default_error_cooldown_min),
            cooldown_regex: raw.cooldown_regex.unwrap_or(d.cooldown_regex),
            log_keep: raw.log_keep.unwrap_or(d.log_keep),
            flock_timeout_ms: raw.flock_timeout_ms.unwrap_or(d.flock_timeout_ms),
            tier_model: raw.tier_model.unwrap_or_default(),
        })
    }
}
