//! 阶梯回落 + 档内 round-robin 挑选算法。
//!
//! 规则：
//! 1. 按 `TIER_ORDER_LOW_TO_HIGH` 从低档到高档扫描（free → student → pro → pro+ → power）。
//! 2. 每个档内用 per-tier cursor 做 round-robin。
//! 3. 低档全部 busy/cooldown 才升档。
//! 4. 档位以 `Profile.kind` 打的标签为准，没打的一律当 free（最先消耗）。

use crate::{tier_rank, State, TIER_ORDER_LOW_TO_HIGH, ZOMBIE_MINUTES};
use chrono::Utc;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Picked {
    pub name: String,
    pub home: PathBuf,
    /// profile 的 tier 标签。未打 tag 时为 "free"（和 pick 选择策略一致）。
    pub kind: String,
    /// 是否為共享 pick（所有 profile 都 in_use 時複用已佔用的）。
    pub shared: bool,
}

#[derive(Debug)]
pub enum PickError {
    AllBusy,
    Empty,
}

impl std::fmt::Display for PickError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PickError::AllBusy => write!(f, "all profiles busy or in cooldown"),
            PickError::Empty => write!(f, "profile pool is empty"),
        }
    }
}
impl std::error::Error for PickError {}

/// 挑一个可用 profile。`dry_run=true` 时只返回挑选结果，不写入 in_use_since /
/// cursor / pick_count，给 `pick --dry-run` 用。
pub fn pick(state: &mut State, pool_dir: &Path, dry_run: bool) -> Result<Picked, PickError> {
    pick_with_zombie(state, pool_dir, ZOMBIE_MINUTES, dry_run)
}

/// 同 pick，但允许自定义 zombie 阈值（给 config.toml 覆盖 + 单测用）。
pub fn pick_with_zombie(
    state: &mut State,
    pool_dir: &Path,
    zombie_minutes: i64,
    dry_run: bool,
) -> Result<Picked, PickError> {
    let now = Utc::now();
    if state.order.is_empty() {
        return Err(PickError::Empty);
    }

    for &tier in TIER_ORDER_LOW_TO_HIGH {
        let candidates: Vec<String> = state
            .order
            .iter()
            .filter(|n| {
                state
                    .profiles
                    .get(*n)
                    .map(|p| tier_rank(p.kind.as_deref()) == tier_rank(Some(tier)))
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
        if candidates.is_empty() {
            continue;
        }
        let n = candidates.len();
        let cur = state.cursors.get(tier).copied().unwrap_or(0) % n;

        for i in 0..n {
            let idx = (cur + i) % n;
            let name = &candidates[idx];
            let p = state
                .profiles
                .get(name)
                .expect("candidate in order/profiles");
            if let Some(cd) = p.cooldown_until {
                if cd > now {
                    continue;
                }
            }
            if let Some(u) = p.in_use_since {
                if (now - u) < chrono::Duration::minutes(zombie_minutes) {
                    continue;
                }
            }
            // 跳過 usage 已耗盡的 profile（resets_at 過期則視為已重置）
            if let Some(ref usage) = p.last_usage {
                if usage.used_percent >= 100.0 {
                    let expired = usage
                        .resets_at
                        .as_deref()
                        .and_then(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
                        .map(|reset_date| now.date_naive() >= reset_date)
                        .unwrap_or(false);
                    if !expired {
                        continue;
                    }
                }
            }
            if !dry_run {
                let entry = state.profiles.get_mut(name).unwrap();
                entry.in_use_since = Some(now);
                entry.in_use_count = 1;
                entry.cooldown_until = None;
                entry.pick_count = entry.pick_count.saturating_add(1);
                state.cursors.insert(tier.to_string(), (idx + 1) % n);
            }
            let kind = state
                .profiles
                .get(name)
                .and_then(|p| p.kind.clone())
                .unwrap_or_else(|| "free".to_string());
            return Ok(Picked {
                name: name.clone(),
                home: pool_dir.join("profiles").join(name),
                kind,
                shared: false,
            });
        }
    }
    // ── fallback: 所有 profile 都 in_use，按階梯順序複用 usage 最低的 ──
    for &tier in TIER_ORDER_LOW_TO_HIGH {
        let mut best: Option<(String, f64, u32)> = None; // (name, usage%, in_use_count)
        for name in &state.order {
            let p = match state.profiles.get(name) {
                Some(p) if tier_rank(p.kind.as_deref()) == tier_rank(Some(tier)) => p,
                _ => continue,
            };
            if p.cooldown_until.is_some_and(|cd| cd > now) {
                continue;
            }
            if p.in_use_since.is_none() {
                continue;
            }
            if let Some(ref usage) = p.last_usage {
                if usage.used_percent >= 100.0 {
                    let expired = usage
                        .resets_at
                        .as_deref()
                        .and_then(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
                        .map(|rd| now.date_naive() >= rd)
                        .unwrap_or(false);
                    if !expired {
                        continue;
                    }
                }
            }
            let pct = p.last_usage.as_ref().map(|u| u.used_percent).unwrap_or(0.0);
            let cnt = p.in_use_count;
            if best.as_ref().is_none_or(|b| (pct, cnt) < (b.1, b.2)) {
                best = Some((name.clone(), pct, cnt));
            }
        }
        if let Some((name, _, _)) = best {
            if !dry_run {
                let entry = state.profiles.get_mut(&name).unwrap();
                entry.in_use_count = entry.in_use_count.saturating_add(1);
                entry.pick_count = entry.pick_count.saturating_add(1);
            }
            let kind = state
                .profiles
                .get(&name)
                .and_then(|p| p.kind.clone())
                .unwrap_or_else(|| "free".to_string());
            return Ok(Picked {
                name: name.clone(),
                home: pool_dir.join("profiles").join(&name),
                kind,
                shared: true,
            });
        }
    }
    Err(PickError::AllBusy)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Profile;
    use chrono::Duration;
    use std::path::PathBuf;

    fn mk_state_tiered(entries: &[(&str, Option<&str>)]) -> State {
        let mut s = State::default();
        for (n, _) in entries {
            s.order.push(n.to_string());
        }
        for (n, tier) in entries {
            let p = Profile {
                kind: tier.map(|t| t.to_string()),
                ..Profile::default()
            };
            s.profiles.insert(n.to_string(), p);
        }
        s
    }

    fn pool() -> PathBuf {
        PathBuf::from("/tmp/kiro-pool-test")
    }

    #[test]
    fn empty_pool_is_empty_error() {
        let mut s = State::default();
        assert!(matches!(
            pick(&mut s, &pool(), false),
            Err(PickError::Empty)
        ));
    }

    #[test]
    fn untagged_is_treated_as_free() {
        let mut s = mk_state_tiered(&[("A", None)]);
        let p = pick(&mut s, &pool(), false).unwrap();
        assert_eq!(p.name, "A");
    }

    #[test]
    fn single_busy_shared_pick() {
        let mut s = mk_state_tiered(&[("A", Some("free"))]);
        s.profiles.get_mut("A").unwrap().in_use_since = Some(Utc::now());
        s.profiles.get_mut("A").unwrap().in_use_count = 1;
        let p = pick(&mut s, &pool(), false).unwrap();
        assert_eq!(p.name, "A");
        assert!(p.shared);
        assert_eq!(s.profiles["A"].in_use_count, 2);
    }

    #[test]
    fn low_tier_consumed_before_high() {
        // 2 free + 1 pro：先轮完 free 才升档到 pro
        let mut s = mk_state_tiered(&[
            ("f1", Some("free")),
            ("p1", Some("pro")),
            ("f2", Some("free")),
        ]);
        let a = pick(&mut s, &pool(), false).unwrap();
        assert_eq!(a.name, "f1");
        let b = pick(&mut s, &pool(), false).unwrap();
        assert_eq!(b.name, "f2");
        // 两个 free 都 busy，升到 pro
        let c = pick(&mut s, &pool(), false).unwrap();
        assert_eq!(c.name, "p1");
        // 都 busy → fallback shared pick，选 free 档 usage 最低的
        let d = pick(&mut s, &pool(), false).unwrap();
        assert!(d.shared);
        // f1 usage 未设（视为 0%），应选 f1
        assert_eq!(d.name, "f1");
    }

    #[test]
    fn per_tier_round_robin_independent() {
        // 2 free + 2 student，交替释放，验证每个档自己的 cursor
        let mut s = mk_state_tiered(&[
            ("f1", Some("free")),
            ("s1", Some("student")),
            ("f2", Some("free")),
            ("s2", Some("student")),
        ]);
        let a = pick(&mut s, &pool(), false).unwrap();
        assert_eq!(a.name, "f1");
        let b = pick(&mut s, &pool(), false).unwrap();
        assert_eq!(b.name, "f2");
        // 两个 free 都 busy → 升到 student
        let c = pick(&mut s, &pool(), false).unwrap();
        assert_eq!(c.name, "s1");
        // 释放 f1，再挑应该回到 free 档（低档优先），而且 free 内部是 f1（cursor 推进到 1，再 mod 2 = 1 对应 f2 已 busy，所以 f1）
        s.profiles.get_mut("f1").unwrap().in_use_since = None;
        let d = pick(&mut s, &pool(), false).unwrap();
        assert_eq!(d.name, "f1");
    }

    #[test]
    fn zombie_in_use_is_released() {
        let mut s = mk_state_tiered(&[("A", Some("free"))]);
        s.profiles.get_mut("A").unwrap().in_use_since = Some(Utc::now() - Duration::minutes(40));
        let p = pick(&mut s, &pool(), false).unwrap();
        assert_eq!(p.name, "A");
    }

    #[test]
    fn expired_cooldown_is_released() {
        let mut s = mk_state_tiered(&[("A", Some("free"))]);
        s.profiles.get_mut("A").unwrap().cooldown_until = Some(Utc::now() - Duration::minutes(1));
        let p = pick(&mut s, &pool(), false).unwrap();
        assert_eq!(p.name, "A");
        assert!(s.profiles["A"].cooldown_until.is_none());
    }

    #[test]
    fn cooldown_low_tier_falls_back_to_high() {
        let mut s = mk_state_tiered(&[("f1", Some("free")), ("p1", Some("pro"))]);
        // free 进入 cooldown
        s.profiles.get_mut("f1").unwrap().cooldown_until = Some(Utc::now() + Duration::minutes(5));
        let p = pick(&mut s, &pool(), false).unwrap();
        assert_eq!(p.name, "p1");
    }
}
