//! `<pool_dir>/config.toml` 运行时配置。缺失或缺字段时回落到 lib.rs 常量。

use crate::{
    DEFAULT_COOLDOWN_REGEX, DEFAULT_ERROR_COOLDOWN_MIN, DEFAULT_FLOCK_TIMEOUT_MS, DEFAULT_LOG_KEEP,
    DEFAULT_USAGE_PREFLIGHT_LOCK_TIMEOUT_MS, DEFAULT_USAGE_PREFLIGHT_MAX_PARALLEL,
    DEFAULT_USAGE_PREFLIGHT_STALE_FORCE_REFRESH_HOURS, DEFAULT_USAGE_PREFLIGHT_TTL_SECS,
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
    pub usage_preflight_enabled: bool,
    pub usage_preflight_ttl_secs: u64,
    pub usage_preflight_lock_timeout_ms: u64,
    /// preflight 并发上限：同时跑几个 kiro-cli /usage 子进程。最小 1（=串行）。
    pub usage_preflight_max_parallel: usize,
    /// used_percent ≥ 100 且 resets_at 缺失时，距离 last_usage.updated_at 超过这个
    /// 小时数就强制 refresh 一次。0 表示永远不强制刷新（回到 v0.2.4 之前的行为）。
    pub usage_preflight_stale_force_refresh_hours: u64,
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
            usage_preflight_enabled: true,
            usage_preflight_ttl_secs: DEFAULT_USAGE_PREFLIGHT_TTL_SECS,
            usage_preflight_lock_timeout_ms: DEFAULT_USAGE_PREFLIGHT_LOCK_TIMEOUT_MS,
            usage_preflight_max_parallel: DEFAULT_USAGE_PREFLIGHT_MAX_PARALLEL,
            usage_preflight_stale_force_refresh_hours:
                DEFAULT_USAGE_PREFLIGHT_STALE_FORCE_REFRESH_HOURS,
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
    usage_preflight_enabled: Option<bool>,
    #[serde(default)]
    usage_preflight_ttl_secs: Option<u64>,
    #[serde(default)]
    usage_preflight_lock_timeout_ms: Option<u64>,
    #[serde(default)]
    usage_preflight_max_parallel: Option<usize>,
    #[serde(default)]
    usage_preflight_stale_force_refresh_hours: Option<u64>,
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
            usage_preflight_enabled: raw
                .usage_preflight_enabled
                .unwrap_or(d.usage_preflight_enabled),
            usage_preflight_ttl_secs: raw
                .usage_preflight_ttl_secs
                .unwrap_or(d.usage_preflight_ttl_secs),
            usage_preflight_lock_timeout_ms: raw
                .usage_preflight_lock_timeout_ms
                .unwrap_or(d.usage_preflight_lock_timeout_ms),
            usage_preflight_max_parallel: raw
                .usage_preflight_max_parallel
                .unwrap_or(d.usage_preflight_max_parallel),
            usage_preflight_stale_force_refresh_hours: raw
                .usage_preflight_stale_force_refresh_hours
                .unwrap_or(d.usage_preflight_stale_force_refresh_hours),
            tier_model: raw.tier_model.unwrap_or_default(),
        })
    }
}
