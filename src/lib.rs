//! kiro-multi 共享库：状态类型、常量、路径辅助函数、平台抽象。

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub mod config;
pub mod pick;
pub mod state;

/// 默认池目录名，相对 $HOME。
pub const DEFAULT_POOL_DIRNAME: &str = ".kiro-pool";
/// 僵尸判定阈值（分钟）——可被 config.toml 覆盖。
pub const ZOMBIE_MINUTES: i64 = 30;
/// --error 默认冷却时长（分钟）——可被 config.toml 覆盖。
pub const DEFAULT_ERROR_COOLDOWN_MIN: i64 = 5;
/// wrap 子进程 stderr / stdout 环形缓冲大小。
pub const STDERR_RING_CAP: usize = 64 * 1024;
/// state.json 当前 schema version；读到更高版本直接报错，更低版本走 serde default 兜底。
pub const SCHEMA_VERSION: u32 = 1;
/// logs/ 默认保留文件数，超过后按 mtime 淘汰最旧。
pub const DEFAULT_LOG_KEEP: usize = 50;
/// state.json flock 等待上限（毫秒）。
pub const DEFAULT_FLOCK_TIMEOUT_MS: u64 = 5000;
/// 默认 cooldown 判定正则。
pub const DEFAULT_COOLDOWN_REGEX: &str = r"(?i)(concurrent|too many|retry in \d+\s?(?:min|minutes?|s|sec|seconds?)|throttl|rate[\s-]?limit|try again later|quota|exceeded)";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfileUsage {
    pub credits_used: f64,
    pub credits_total: f64,
    pub used_percent: f64,
    pub plan: Option<String>,
    pub resets_at: Option<String>,
    /// 此 usage 數據寫入的時間。
    #[serde(default)]
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Profile {
    #[serde(default)]
    pub in_use_since: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub cooldown_until: Option<chrono::DateTime<chrono::Utc>>,
    /// 显式打的类型标签："free" / "student" / "pro" / "pro+" / "power"。
    /// 没打时 list 会回退到 token JSON 的 Builder-ID-vs-Identity-Center 推断。
    #[serde(default)]
    pub kind: Option<String>,
    /// 當前共享此 profile 的 session 數。歸零時清 in_use_since。
    #[serde(default)]
    pub in_use_count: u32,
    /// 累计被 pick 次数（含 dry-run 不计）。
    #[serde(default)]
    pub pick_count: u64,
    /// 累计触发 cooldown 次数。
    #[serde(default)]
    pub cooldown_count: u64,
    /// 最近一次 usage 查询结果。
    #[serde(default)]
    pub last_usage: Option<ProfileUsage>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    /// schema version；旧 state.json 没这字段时 serde 会回填 0，代码当 v0 处理（结构兼容）。
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub order: Vec<String>,
    /// per-tier 轮转游标。key 是 tier 名，value 是下次在该档内候选列表的起始下标。
    #[serde(default)]
    pub cursors: BTreeMap<String, usize>,
    /// 旧单一 cursor，保留字段仅为兼容旧 state.json；pick 不再读它。
    #[serde(default)]
    pub cursor: usize,
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
}

/// 档位优先级：从低到高；pick 按这个顺序逐档尝试（阶梯回落）。
pub const TIER_ORDER_LOW_TO_HIGH: &[&str] = &["free", "student", "pro", "pro+", "power"];

/// 返回 tier 的排序键。未知/未打标签的档位一律当 free（最先消耗）。
pub fn tier_rank(kind: Option<&str>) -> usize {
    let k = kind.unwrap_or("free");
    TIER_ORDER_LOW_TO_HIGH
        .iter()
        .position(|t| *t == k)
        .unwrap_or(0)
}

/// 解析 pool-dir 的优先级：CLI > `KIRO_POOL_DIR` 环境变量 > `$HOME/.kiro-pool`。
pub fn resolve_pool_dir(override_path: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = override_path {
        return Ok(p.to_path_buf());
    }
    if let Ok(v) = std::env::var("KIRO_POOL_DIR") {
        if !v.is_empty() {
            return Ok(PathBuf::from(v));
        }
    }
    let home = std::env::var("HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(dirs_home)
        .ok_or_else(|| anyhow!("cannot determine $HOME"))?;
    Ok(home.join(DEFAULT_POOL_DIRNAME))
}

/// 校验 profile 名：允许 ASCII 字母/数字/下划线/横线，不能为空。
pub fn valid_profile_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 64
        && n.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// 给定池目录，返回某 profile 的 HOME 路径。
pub fn profile_home(pool_dir: &Path, name: &str) -> PathBuf {
    pool_dir.join("profiles").join(name)
}

/// kiro-cli 数据目录相对 HOME 的子路径（跨平台差异）。
/// macOS: `Library/Application Support/kiro-cli`
/// Linux / 其他 Unix: `.local/share/kiro-cli`（XDG_DATA_HOME 惯例）
pub fn kiro_data_relpath() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "Library/Application Support/kiro-cli"
    }
    #[cfg(not(target_os = "macos"))]
    {
        ".local/share/kiro-cli"
    }
}

/// profile 内 kiro-cli 的 sqlite 数据库绝对路径。
pub fn profile_sqlite(pool_dir: &Path, name: &str) -> PathBuf {
    profile_home(pool_dir, name)
        .join(kiro_data_relpath())
        .join("data.sqlite3")
}

/// 确保 profile HOME 下有独立 keychain。
///
/// macOS：创建并解锁 per-profile login.keychain-db（Kiro 用 macOS Keychain 存 token，
/// 服务名全局固定，不 per-profile 独立会互相覆盖）。
///
/// **重要**：`security create-keychain` 默认会把新 keychain 加入调用者的 user keychain
/// search list。user search list 路径取决于 `$HOME/Library/Preferences/com.apple.security.plist`。
/// 如果不 override HOME，profile keychain 会污染真实 HOME 的 search list，导致直接运行
/// kiro-cli 时串台读到 profile 的 token。所以这里强制把 HOME 指向 profile_home，让
/// search list 改动只写到 profile 的 prefs 文件里。
///
/// Linux / 其他：no-op。kiro-cli 在 Linux 下把 token 写在 sqlite 里，HOME 隔离就够了。
#[cfg(target_os = "macos")]
pub fn ensure_keychain(profile_home: &Path) -> Result<()> {
    let kc_dir = profile_home.join("Library/Keychains");
    std::fs::create_dir_all(&kc_dir).with_context(|| format!("mkdir {}", kc_dir.display()))?;
    // 同時確保 profile Prefs 目錄存在，security 寫 user search list 會用到
    let prefs_dir = profile_home.join("Library/Preferences");
    std::fs::create_dir_all(&prefs_dir)
        .with_context(|| format!("mkdir {}", prefs_dir.display()))?;

    let kc = kc_dir.join("login.keychain-db");
    if !kc.exists() {
        let ok = std::process::Command::new("security")
            .args(["create-keychain", "-p", ""])
            .arg(&kc)
            .env("HOME", profile_home)
            .status()
            .with_context(|| "spawn security create-keychain")?
            .success();
        if !ok {
            return Err(anyhow!(
                "security create-keychain failed for {}",
                kc.display()
            ));
        }
        let _ = std::process::Command::new("security")
            .args(["set-keychain-settings"])
            .arg(&kc)
            .env("HOME", profile_home)
            .status();
    }
    let ok = std::process::Command::new("security")
        .args(["unlock-keychain", "-p", ""])
        .arg(&kc)
        .env("HOME", profile_home)
        .status()
        .with_context(|| "spawn security unlock-keychain")?
        .success();
    if !ok {
        return Err(anyhow!(
            "security unlock-keychain failed for {}",
            kc.display()
        ));
    }
    Ok(())
}

/// 從真實 HOME 的 user keychain search list 移除所有指向 pool_dir 下的 keychain。
/// 用來清理舊版 `ensure_keychain` 已經造成的污染。返回清掉的條目數。
///
/// 實現：`security list-keychains -d user` 讀當前 list，過濾掉路徑以 pool_dir 開頭的條目，
/// 再用 `security list-keychains -d user -s <kept...>` 寫回。
#[cfg(target_os = "macos")]
pub fn cleanup_user_keychain_search_list(pool_dir: &Path) -> Result<usize> {
    let output = std::process::Command::new("security")
        .args(["list-keychains", "-d", "user"])
        .output()
        .with_context(|| "spawn security list-keychains")?;
    if !output.status.success() {
        return Err(anyhow!("security list-keychains -d user failed"));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let entries: Vec<String> = stdout
        .lines()
        .filter_map(|l| {
            let t = l.trim();
            let t = t.strip_prefix('"')?.strip_suffix('"')?;
            Some(t.to_string())
        })
        .collect();

    let pool_str = pool_dir.to_string_lossy().to_string();
    let kept: Vec<String> = entries
        .iter()
        .filter(|e| !e.starts_with(&pool_str))
        .cloned()
        .collect();
    let removed = entries.len().saturating_sub(kept.len());
    if removed > 0 {
        let mut cmd = std::process::Command::new("security");
        cmd.args(["list-keychains", "-d", "user", "-s"]);
        for k in &kept {
            cmd.arg(k);
        }
        let ok = cmd
            .status()
            .with_context(|| "spawn security list-keychains -s")?
            .success();
        if !ok {
            return Err(anyhow!("security list-keychains -s failed"));
        }
    }
    Ok(removed)
}

#[cfg(not(target_os = "macos"))]
pub fn cleanup_user_keychain_search_list(_pool_dir: &Path) -> Result<usize> {
    Ok(0)
}

#[cfg(not(target_os = "macos"))]
pub fn ensure_keychain(_profile_home: &Path) -> Result<()> {
    Ok(())
}

/// 把 kiro-cli 的共享运行时资源（bun / tui.js / shell/ 等）从真实 HOME
/// symlink 进 profile HOME，仅保留 data.sqlite3 / history / knowledge_bases 为 per-profile。
pub fn ensure_shared_assets(profile_home: &Path) -> Result<()> {
    let real_home = std::env::var("HOME").context("HOME not set")?;
    let real_src = Path::new(&real_home).join(kiro_data_relpath());
    if !real_src.exists() {
        return Err(anyhow!(
            "real kiro-cli install dir not found: {} — 先在真实 HOME 下至少跑一次 kiro-cli 让它初始化",
            real_src.display()
        ));
    }
    let dst = profile_home.join(kiro_data_relpath());
    std::fs::create_dir_all(&dst)?;

    const PER_PROFILE: &[&str] = &[
        "data.sqlite3",
        "data.sqlite3-journal",
        "data.sqlite3-wal",
        "data.sqlite3-shm",
        "history",
        "knowledge_bases",
    ];

    for entry in std::fs::read_dir(&real_src)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy().to_string();
        if PER_PROFILE.iter().any(|p| *p == name_str) {
            continue;
        }
        let link = dst.join(&name);
        // 旧版 wrap 或首次 kiro-cli 启动可能在 profile 下留 stub dir / file，它们会屏蔽到真源的
        // symlink，导致 agent / shared asset 看不到。把非 symlink 的旧条目清掉再重建。
        // 注意：如果用戶曾在 profile HOME 下手動放過文件/目錄，這裡會靜默刪掉 —
        // profile HOME 按設計是 ephemeral 的，一切從真實 HOME 映射；手動改動會在
        // 下次 ensure 時被覆蓋。刪除前打一行 stderr 警告，讓用戶能看見發生了什麼。
        match link.symlink_metadata() {
            Err(_) => {}
            Ok(m) if m.file_type().is_symlink() => continue,
            Ok(m) if m.is_dir() => {
                eprintln!(
                    "kiro-pool: replacing stale dir with symlink: {} (content will be discarded)",
                    link.display()
                );
                std::fs::remove_dir_all(&link)
                    .with_context(|| format!("remove stale dir {}", link.display()))?;
            }
            Ok(_) => {
                eprintln!(
                    "kiro-pool: replacing stale file with symlink: {}",
                    link.display()
                );
                std::fs::remove_file(&link)
                    .with_context(|| format!("remove stale file {}", link.display()))?;
            }
        }
        std::os::unix::fs::symlink(entry.path(), &link)
            .with_context(|| format!("symlink {} -> {}", link.display(), entry.path().display()))?;
    }
    Ok(())
}

/// 把 kiro-cli 的兄弟二进制从真实 `~/.local/bin` symlink 进 profile 的 `.local/bin`。
/// kiro-cli 通过 `$HOME/.local/bin/kiro-cli-chat` 定位 chat 子二进制。
pub fn ensure_sibling_binaries(profile_home: &Path) -> Result<()> {
    let real_home = std::env::var("HOME").context("HOME not set")?;
    let src_dir = Path::new(&real_home).join(".local/bin");
    if !src_dir.exists() {
        return Ok(());
    }
    let dst_dir = profile_home.join(".local/bin");
    std::fs::create_dir_all(&dst_dir)?;
    for name in ["kiro-cli", "kiro-cli-chat", "kiro-cli-term"] {
        let src = src_dir.join(name);
        if !src.exists() {
            continue;
        }
        let link = dst_dir.join(name);
        if link.symlink_metadata().is_ok() {
            continue;
        }
        std::os::unix::fs::symlink(&src, &link)
            .with_context(|| format!("symlink {} -> {}", link.display(), src.display()))?;
    }
    Ok(())
}

/// 把用户 `~/.kiro/` 下的 agent / skills / settings / memory.md symlink 进 profile HOME。
/// `sessions/` 和 `.cli_bash_history` 保持 per-profile。
pub fn ensure_kiro_config(profile_home: &Path) -> Result<()> {
    let real_home = std::env::var("HOME").context("HOME not set")?;
    let real_src = Path::new(&real_home).join(".kiro");
    if !real_src.exists() {
        return Ok(());
    }
    let dst = profile_home.join(".kiro");
    std::fs::create_dir_all(&dst)?;

    const PER_PROFILE: &[&str] = &["sessions", ".cli_bash_history"];

    for entry in std::fs::read_dir(&real_src)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy().to_string();
        if PER_PROFILE.iter().any(|p| *p == name_str) {
            continue;
        }
        let link = dst.join(&name);
        // 旧版 wrap 或首次 kiro-cli 启动可能在 profile 下留 stub dir / file，它们会屏蔽到真源的
        // symlink，导致 agent / shared asset 看不到。把非 symlink 的旧条目清掉再重建。
        // 注意：如果用戶曾在 profile HOME 下手動放過文件/目錄，這裡會靜默刪掉 —
        // profile HOME 按設計是 ephemeral 的，一切從真實 HOME 映射；手動改動會在
        // 下次 ensure 時被覆蓋。刪除前打一行 stderr 警告，讓用戶能看見發生了什麼。
        match link.symlink_metadata() {
            Err(_) => {}
            Ok(m) if m.file_type().is_symlink() => continue,
            Ok(m) if m.is_dir() => {
                eprintln!(
                    "kiro-pool: replacing stale dir with symlink: {} (content will be discarded)",
                    link.display()
                );
                std::fs::remove_dir_all(&link)
                    .with_context(|| format!("remove stale dir {}", link.display()))?;
            }
            Ok(_) => {
                eprintln!(
                    "kiro-pool: replacing stale file with symlink: {}",
                    link.display()
                );
                std::fs::remove_file(&link)
                    .with_context(|| format!("remove stale file {}", link.display()))?;
            }
        }
        std::os::unix::fs::symlink(entry.path(), &link)
            .with_context(|| format!("symlink {} -> {}", link.display(), entry.path().display()))?;
    }
    Ok(())
}

/// fetch_profile_usage 的默認 timeout（秒）。超過就 kill 子進程避免無限 hang。
pub const USAGE_FETCH_TIMEOUT_SECS: u64 = 30;

/// spawn `kiro-cli chat --no-interactive /usage` 並解析輸出拿 usage 數據。
///
/// kiro-cli 把 `/usage` 面板寫到 stderr（slash command 不是 chat 正文），
/// 且帶 ANSI 控制字符。stdout/stderr 都讀，strip ANSI 後解析。
///
/// 帶 `USAGE_FETCH_TIMEOUT_SECS` timeout，防止 kiro-cli 卡住（網絡問題 / token
/// 過期要交互登錄）讓 `kiro-pool usage` / `list --refresh-usage` 整條命令 hang。
pub fn fetch_profile_usage(pool_dir: &Path, name: &str) -> Option<ProfileUsage> {
    let home = profile_home(pool_dir, name);
    let mut child = std::process::Command::new("kiro-cli")
        .args(["chat", "--no-interactive", "/usage"])
        .env("HOME", &home)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(USAGE_FETCH_TIMEOUT_SECS);
    loop {
        match child.try_wait().ok()? {
            Some(_) => break,
            None => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    eprintln!(
                        "kiro-pool: fetch_profile_usage({name}) timed out after {}s",
                        USAGE_FETCH_TIMEOUT_SECS
                    );
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
    let output = child.wait_with_output().ok()?;
    let raw = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let text = strip_ansi(&raw);
    parse_usage_output(&text)
}

/// 剝離 ANSI CSI 序列，保留純文本。
fn strip_ansi(s: &str) -> String {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"\x1b\[[0-9;]*[A-Za-z]").unwrap());
    re.replace_all(s, "").into_owned()
}

/// 解析 kiro-cli `/usage` 輸出。實際格式（strip ANSI 後）：
/// ```text
/// Estimated Usage | resets on 2026-05-01 | KIRO STUDENT
/// Credits (951.38 of 1000 covered in plan)
/// ████████████████████████████████████████ 95%
/// Overages: Disabled
/// ```
fn parse_usage_output(text: &str) -> Option<ProfileUsage> {
    let mut usage = ProfileUsage::default();

    // Credits (951.38 of 1000 covered in plan)
    let re_credits = regex::Regex::new(r"Credits\s*\(\s*([\d.]+)\s*of\s*([\d.]+)").ok()?;
    // resets on 2026-05-01
    let re_reset = regex::Regex::new(r"resets\s+on\s+(\d{4}-\d{2}-\d{2})").ok()?;
    // 首行尾部 plan：| KIRO STUDENT
    let re_plan = regex::Regex::new(r"\|\s*([A-Z][A-Z0-9 ]+?)\s*$").ok()?;

    if let Some(caps) = re_credits.captures(text) {
        let used: f64 = caps.get(1)?.as_str().parse().ok()?;
        let total: f64 = caps.get(2)?.as_str().parse().ok()?;
        usage.credits_used = used;
        usage.credits_total = total;
        usage.used_percent = if total > 0.0 {
            (used / total * 100.0).clamp(0.0, 100.0)
        } else {
            0.0
        };
    }

    if let Some(caps) = re_reset.captures(text) {
        usage.resets_at = Some(caps[1].to_string());
    }

    // plan 從含 "Estimated Usage" 的行取
    usage.plan = text
        .lines()
        .find(|l| l.contains("Estimated Usage"))
        .and_then(|l| re_plan.captures(l))
        .map(|c| c[1].trim().to_string());

    if usage.credits_total > 0.0 || usage.plan.is_some() {
        usage.updated_at = Some(chrono::Utc::now());
        Some(usage)
    } else {
        None
    }
}

/// 用 SQLite 在線 backup API 複製數據庫。源可以正在被另一進程寫（WAL 模式下），
/// 通過 SQLite 內部的 page lock 機制保證拿到一致快照。
fn backup_sqlite(src: &Path, dst: &Path) -> Result<()> {
    let src_conn =
        rusqlite::Connection::open_with_flags(src, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("open source sqlite {}", src.display()))?;
    let mut dst_conn = rusqlite::Connection::open(dst)
        .with_context(|| format!("create dest sqlite {}", dst.display()))?;
    let backup =
        rusqlite::backup::Backup::new(&src_conn, &mut dst_conn).context("start sqlite backup")?;
    backup
        .run_to_completion(256, std::time::Duration::from_millis(10), None)
        .context("run sqlite backup")?;
    Ok(())
}

/// 掃描 `pool_dir/profiles/` 下所有 `*__shared_<pid>` 臨時目錄，
/// 對每個目錄檢查對應的 pid 是否還活著（`kill(pid, 0)`）。死掉的就清掉。
///
/// 用來處理上一次 wrap 被 SIGKILL / OOM / 斷電導致的 stale shared profile 目錄。
pub fn cleanup_stale_shared_profiles(pool_dir: &Path) -> Result<usize> {
    let profiles_dir = pool_dir.join("profiles");
    if !profiles_dir.exists() {
        return Ok(0);
    }
    let mut cleaned = 0;
    for entry in std::fs::read_dir(&profiles_dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        // 格式: <source>__shared_<pid>
        let Some((_source, pid_str)) = name.rsplit_once("__shared_") else {
            continue;
        };
        let Ok(pid) = pid_str.parse::<i32>() else {
            continue;
        };
        // kill(pid, 0) 只檢查進程是否存在，不發信號。errno == ESRCH 表示 no such process。
        let alive = unsafe { libc::kill(pid, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
        if alive {
            continue;
        }
        let path = entry.path();
        if std::fs::remove_dir_all(&path).is_ok() {
            cleaned += 1;
        }
    }
    Ok(cleaned)
}

/// 为 shared pick 创建临时 profile 目录。
///
/// 认证文件（keychain / sqlite token）从原 profile 共享，
/// 其余（sqlite 写入、pid file、sessions）独立，避免 kiro-cli 的 PID file 互斥。
pub fn create_shared_profile(pool_dir: &Path, source_name: &str, pid: u32) -> Result<PathBuf> {
    let src = profile_home(pool_dir, source_name);
    let shared_name = format!("{}__shared_{}", source_name, pid);
    let dst = pool_dir.join("profiles").join(&shared_name);
    if dst.exists() {
        std::fs::remove_dir_all(&dst)?;
    }

    // Keychain（macOS）：整個目錄 symlink
    #[cfg(target_os = "macos")]
    {
        let kc_src = src.join("Library/Keychains");
        if kc_src.exists() {
            let kc_dst = dst.join("Library/Keychains");
            std::fs::create_dir_all(kc_dst.parent().unwrap())?;
            std::os::unix::fs::symlink(&kc_src, &kc_dst)?;
        }
    }

    // kiro-cli data dir：symlink 共享資源，複製 sqlite（認證數據在裡面，但不能共享寫鎖）
    let data_rel = kiro_data_relpath();
    let data_src = src.join(data_rel);
    let data_dst = dst.join(data_rel);
    std::fs::create_dir_all(&data_dst)?;
    if data_src.exists() {
        for entry in std::fs::read_dir(&data_src)? {
            let entry = entry?;
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            let link = data_dst.join(&name);
            if link.symlink_metadata().is_ok() {
                continue;
            }
            if name_str == "data.sqlite3" {
                // 用 SQLite 在線備份 API 代替 fs::copy：源庫在 WAL 模式下可能正被
                // 另一個 kiro-cli 進程寫，fs::copy 不是 SQLite-aware，可能拿到半寫快照
                // 導致 auth token 損壞。Backup API 會走 SQLite 自己的 page lock 機制，
                // 保證讀到一致狀態。
                backup_sqlite(&entry.path(), &link).with_context(|| {
                    format!(
                        "sqlite backup {} -> {}",
                        entry.path().display(),
                        link.display()
                    )
                })?;
                continue;
            }
            // WAL/SHM/journal 不複製（新 sqlite 會自建）
            if name_str.starts_with("data.sqlite3-") {
                continue;
            }
            let target = std::fs::canonicalize(entry.path()).unwrap_or_else(|_| entry.path());
            std::os::unix::fs::symlink(&target, &link)?;
        }
    }

    // .local/bin, .kiro — 整個 symlink
    for sub in [".local/bin", ".kiro"] {
        let s = src.join(sub);
        if s.exists() {
            let d = dst.join(sub);
            std::fs::create_dir_all(d.parent().unwrap())?;
            std::os::unix::fs::symlink(&s, &d)?;
        }
    }

    // .semantic_search — symlink
    let ss = src.join(".semantic_search");
    if ss.exists() {
        std::os::unix::fs::symlink(&ss, dst.join(".semantic_search"))?;
    }

    Ok(dst)
}

/// 从 profile sqlite 的 `kirocli:odic:token` JSON 读出 license 类型。
pub fn profile_license(pool_dir: &Path, name: &str) -> Option<&'static str> {
    let db = profile_sqlite(pool_dir, name);
    if !db.exists() {
        return None;
    }
    let conn =
        rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()?;
    let raw: String = conn
        .query_row(
            "SELECT value FROM auth_kv WHERE key = 'kirocli:odic:token'",
            [],
            |row| row.get(0),
        )
        .ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let start_url = v.get("start_url").and_then(|x| x.as_str());
    match start_url {
        None | Some("") => Some("free"),
        Some(u) if u.contains("view.awsapps.com") => Some("free"),
        Some(_) => Some("pro"),
    }
}

/// 解析 "5m" / "30s" / "1h" 为 chrono::Duration。
pub fn parse_duration(s: &str) -> Result<chrono::Duration> {
    let s = s.trim();
    if s.is_empty() {
        return Err(anyhow!("empty duration"));
    }
    let (num_part, unit) = s.split_at(
        s.find(|c: char| !c.is_ascii_digit())
            .ok_or_else(|| anyhow!("missing unit in duration: {s}"))?,
    );
    let n: i64 = num_part
        .parse()
        .map_err(|_| anyhow!("invalid number in duration: {s}"))?;
    let d = match unit {
        "s" | "sec" | "secs" | "seconds" => chrono::Duration::seconds(n),
        "m" | "min" | "mins" | "minutes" => chrono::Duration::minutes(n),
        "h" | "hr" | "hour" | "hours" => chrono::Duration::hours(n),
        "d" | "day" | "days" => chrono::Duration::days(n),
        other => return Err(anyhow!("unknown duration unit: {other}")),
    };
    Ok(d)
}

/// 按 mtime 保留最新 keep 个文件，其余删除。
pub fn rotate_logs(log_dir: &Path, keep: usize) -> Result<()> {
    if !log_dir.exists() {
        return Ok(());
    }
    let mut entries: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(log_dir)?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            if !m.is_file() {
                return None;
            }
            Some((m.modified().ok()?, e.path()))
        })
        .collect();
    if entries.len() <= keep {
        return Ok(());
    }
    entries.sort_by_key(|e| std::cmp::Reverse(e.0));
    for (_, p) in entries.into_iter().skip(keep) {
        let _ = std::fs::remove_file(&p);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_duration_forms() {
        assert_eq!(parse_duration("5m").unwrap(), chrono::Duration::minutes(5));
        assert_eq!(
            parse_duration("30s").unwrap(),
            chrono::Duration::seconds(30)
        );
        assert_eq!(parse_duration("1h").unwrap(), chrono::Duration::hours(1));
        assert!(parse_duration("").is_err());
        assert!(parse_duration("10").is_err());
        assert!(parse_duration("10x").is_err());
    }

    #[test]
    fn valid_profile_name_cases() {
        assert!(valid_profile_name("a"));
        assert!(valid_profile_name("A_1-2"));
        assert!(!valid_profile_name(""));
        assert!(!valid_profile_name("a/b"));
        assert!(!valid_profile_name("a.b"));
        assert!(!valid_profile_name(&"x".repeat(65)));
    }
}
