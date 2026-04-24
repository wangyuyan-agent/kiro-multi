//! kiro-pool CLI：登录、列表、挑选、释放、移除、自检、统计、补全。

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::{generate, Shell};
use kiro_pool::{
    config::Config,
    fetch_profile_usage, parse_duration,
    pick::pick,
    profile_home, profile_license, profile_sqlite, resolve_pool_dir, rotate_logs,
    state::{read_state, with_state},
    valid_profile_name, Profile,
};
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

const TIER_VALUES: [&str; 5] = ["free", "student", "pro", "pro+", "power"];

#[derive(Parser)]
#[command(name = "kiro-pool", version, about = "Kiro 多账号本地轮转池")]
struct Cli {
    /// 池目录（默认 $HOME/.kiro-pool，也可用 KIRO_POOL_DIR 环境变量覆盖）
    #[arg(long, global = true)]
    pool_dir: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 登录一个 profile（device flow）
    Login {
        name: String,
        /// 订阅档位标签：free / student / pro / pro+ / power
        #[arg(long, default_value = "free",
              value_parser = clap::builder::PossibleValuesParser::new(TIER_VALUES))]
        tier: String,
        /// 给出即走 IAM Identity Center 流；留空则走 Builder ID 流
        #[arg(long)]
        identity_provider: Option<String>,
        #[arg(long, default_value = "us-east-1")]
        region: String,
    },
    /// 给已登录 profile 打/改档位标签
    Tag {
        name: String,
        #[arg(value_parser = clap::builder::PossibleValuesParser::new(TIER_VALUES))]
        kind: String,
    },
    /// 列出池中所有 profile 的状态
    List {
        /// JSON 格式输出（scripting 友好）
        #[arg(long)]
        json: bool,
        /// 实时查询各 profile 的 usage 并写入 state（慢）
        #[arg(long)]
        refresh_usage: bool,
    },
    /// 原子挑一个 profile 并标记 in_use
    Pick {
        #[arg(long)]
        json: bool,
        /// 只预览选谁，不写 in_use_since / cursor / pick_count
        #[arg(long)]
        dry_run: bool,
    },
    /// 释放 profile；可选冷却
    Release {
        name: String,
        #[arg(long)]
        cooldown: Option<String>,
        #[arg(long)]
        error: bool,
    },
    /// 手动清 profile 的 cooldown（误判时复活用）
    ClearCooldown {
        /// 要清的 profile；--all 时忽略
        name: Option<String>,
        /// 清所有 profile 的 cooldown
        #[arg(long)]
        all: bool,
    },
    /// 从池中移除 profile
    Remove {
        name: String,
        #[arg(long)]
        purge: bool,
    },
    /// 登出 profile（清 auth 数据，保留目录和 state 记录）
    Logout { name: String },
    /// 自检：检查安装 / 符号链接 / sqlite / keychain
    Doctor {
        /// 只检某个 profile；缺省检所有
        name: Option<String>,
    },
    /// 累计 pick / cooldown 计数
    Stats {
        #[arg(long)]
        json: bool,
    },
    /// 查询各 profile 的 usage / credits 用量
    Usage {
        /// JSON 格式输出
        #[arg(long)]
        json: bool,
        /// 把查到的 usage 写入 state.json 的 last_usage 字段
        #[arg(long)]
        update_state: bool,
        /// 只查某个 profile
        name: Option<String>,
    },
    /// 清理 user keychain search list 中指向 pool_dir 的 profile keychain
    /// （修復舊版 ensure_keychain 污染：直接跑 kiro-cli 時誤讀 profile token）
    #[cfg(target_os = "macos")]
    FixKeychain {
        /// 只檢查，不實際修改
        #[arg(long)]
        dry_run: bool,
    },
    /// 打印 shell 补全脚本到 stdout
    Completion {
        #[arg(value_enum)]
        shell: Shell,
    },
}

fn main() -> ExitCode {
    match run() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("kiro-pool: {e:#}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    // completion 不需要 pool_dir，也不应该创建目录
    if let Cmd::Completion { shell } = cli.cmd {
        let mut cmd = Cli::command();
        let name = cmd.get_name().to_string();
        generate(shell, &mut cmd, name, &mut io::stdout());
        return Ok(ExitCode::SUCCESS);
    }
    let pool_dir = resolve_pool_dir(cli.pool_dir.as_deref())?;
    fs::create_dir_all(&pool_dir).with_context(|| format!("create {}", pool_dir.display()))?;
    let cfg = Config::load(&pool_dir)?;
    if cfg.flock_timeout_ms != kiro_pool::DEFAULT_FLOCK_TIMEOUT_MS {
        std::env::set_var(
            "KIRO_POOL_FLOCK_TIMEOUT_MS",
            cfg.flock_timeout_ms.to_string(),
        );
    }
    match cli.cmd {
        Cmd::Login {
            name,
            tier,
            identity_provider,
            region,
        } => cmd_login(
            &pool_dir,
            &name,
            &tier,
            identity_provider.as_deref(),
            &region,
        ),
        Cmd::List {
            json,
            refresh_usage,
        } => cmd_list(&pool_dir, &cfg, json, refresh_usage),
        Cmd::Pick { json, dry_run } => cmd_pick(&pool_dir, json, dry_run),
        Cmd::Release {
            name,
            cooldown,
            error,
        } => cmd_release(&pool_dir, &cfg, &name, cooldown.as_deref(), error),
        Cmd::ClearCooldown { name, all } => cmd_clear_cooldown(&pool_dir, name.as_deref(), all),
        Cmd::Remove { name, purge } => cmd_remove(&pool_dir, &name, purge),
        Cmd::Logout { name } => cmd_logout(&pool_dir, &name),
        Cmd::Tag { name, kind } => cmd_tag(&pool_dir, &name, &kind),
        Cmd::Doctor { name } => cmd_doctor(&pool_dir, name.as_deref()),
        Cmd::Stats { json } => cmd_stats(&pool_dir, json),
        Cmd::Usage {
            json,
            update_state,
            name,
        } => cmd_usage(&pool_dir, json, update_state, name.as_deref()),
        #[cfg(target_os = "macos")]
        Cmd::FixKeychain { dry_run } => cmd_fix_keychain(&pool_dir, dry_run),
        Cmd::Completion { .. } => unreachable!(),
    }
}

#[cfg(target_os = "macos")]
fn cmd_fix_keychain(pool_dir: &Path, dry_run: bool) -> Result<ExitCode> {
    // 先列出被污染的條目
    let output = std::process::Command::new("security")
        .args(["list-keychains", "-d", "user"])
        .output()
        .context("spawn security list-keychains")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let pool_str = pool_dir.to_string_lossy().to_string();
    let polluted: Vec<String> = stdout
        .lines()
        .filter_map(|l| {
            let t = l.trim();
            let t = t.strip_prefix('"')?.strip_suffix('"')?;
            Some(t.to_string())
        })
        .filter(|e| e.starts_with(&pool_str))
        .collect();

    if polluted.is_empty() {
        println!("user keychain search list is clean (no entries under {pool_str})");
        return Ok(ExitCode::SUCCESS);
    }

    println!(
        "found {} polluted entries in user keychain search list:",
        polluted.len()
    );
    for p in &polluted {
        println!("  - {p}");
    }

    if dry_run {
        println!("(dry-run: not modified)");
        return Ok(ExitCode::SUCCESS);
    }

    let removed = kiro_pool::cleanup_user_keychain_search_list(pool_dir)?;
    println!("removed {removed} entries. current search list:");
    let output = std::process::Command::new("security")
        .args(["list-keychains", "-d", "user"])
        .output()?;
    print!("{}", String::from_utf8_lossy(&output.stdout));
    Ok(ExitCode::SUCCESS)
}

fn cmd_tag(pool_dir: &Path, name: &str, kind: &str) -> Result<ExitCode> {
    with_state(pool_dir, |s| {
        let p = s
            .profiles
            .get_mut(name)
            .ok_or_else(|| anyhow!("no such profile: {name}"))?;
        p.kind = Some(kind.to_string());
        Ok(())
    })?;
    println!("{} tagged as {}", name, kind);
    Ok(ExitCode::SUCCESS)
}

fn cmd_login(
    pool_dir: &Path,
    name: &str,
    tier: &str,
    idp: Option<&str>,
    region: &str,
) -> Result<ExitCode> {
    if !valid_profile_name(name) {
        return Err(anyhow!(
            "invalid profile name {name:?}: use [A-Za-z0-9_-], 1-64 chars"
        ));
    }
    let home = profile_home(pool_dir, name);
    kiro_pool::ensure_keychain(&home)?;
    kiro_pool::ensure_shared_assets(&home)?;
    kiro_pool::ensure_sibling_binaries(&home)?;
    kiro_pool::ensure_kiro_config(&home)?;

    let mut cmd = Command::new("kiro-cli");
    cmd.arg("login")
        .arg("--region")
        .arg(region)
        .env("HOME", &home)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if let Some(ip) = idp {
        cmd.arg("--identity-provider").arg(ip);
    }
    let want_kind = tier.to_string();

    // spawn kiro-cli（阻塞等 OAuth callback）
    let mut child = cmd.spawn().context("spawn kiro-cli login")?;
    let _child_id = child.id();

    // 後台線程：等 kiro-cli 起好 listener 後，提示用戶貼 callback URL
    // kiro-cli 選完登錄方式後會監聽 localhost:3128，我們輪詢檢測
    use std::net::TcpStream;
    use std::thread;
    let relay_t = thread::spawn(move || {
        // 等 localhost:3128 有 listener（最多等 60 秒）
        let mut found = false;
        for _ in 0..120 {
            thread::sleep(std::time::Duration::from_millis(500));
            if TcpStream::connect("127.0.0.1:3128").is_ok() {
                found = true;
                break;
            }
        }
        if !found {
            return;
        }
        // 給用戶時間看到 auth URL
        thread::sleep(std::time::Duration::from_secs(3));
        eprintln!();
        eprintln!("─── kiro-pool OAuth helper ───");
        eprintln!(
            "瀏覽器完成登錄後，地址欄會跳轉到 http://localhost:3128/...（頁面無法打開是正常的）。"
        );
        eprintln!("請複製地址欄的完整 URL，貼到這裡按 Enter：");
        let mut input = String::new();
        if io::stdin().read_line(&mut input).is_ok() {
            let url = input.trim();
            if !url.is_empty() && (url.contains("localhost") || url.contains("127.0.0.1")) {
                let curl_url = url.replace("https://localhost", "http://localhost");
                eprintln!("正在回傳 callback...");
                let _ = std::process::Command::new("curl")
                    .args(["-s", "-o", "/dev/null"])
                    .arg(&curl_url)
                    .status();
            }
        }
    });

    let status = child.wait().context("wait kiro-cli login")?;
    // kiro-cli 已退出（收到 callback 或超時），relay 線程可能還阻塞在 stdin read_line —
    // Rust 沒有 kill thread 的辦法。為了讓 login 命令及時返回 shell，成功路徑走
    // std::process::exit(0) 繞開 relay 線程；失敗路徑直接返回 exit code。
    drop(relay_t);

    if !status.success() {
        return Ok(ExitCode::from(status.code().unwrap_or(1) as u8));
    }

    with_state(pool_dir, |s| {
        if !s.order.iter().any(|n| n == name) {
            s.order.push(name.to_string());
        }
        let p = s
            .profiles
            .entry(name.to_string())
            .or_insert_with(Profile::default);
        p.kind = Some(want_kind.clone());
        Ok(())
    })?;
    println!("profile {} ready", name);
    // 強制退出，不等 relay 線程
    std::process::exit(0);
}

fn cmd_list(pool_dir: &Path, cfg: &Config, json: bool, refresh_usage: bool) -> Result<ExitCode> {
    if refresh_usage {
        let names: Vec<String> = read_state(pool_dir)?.order.clone();
        let mut results: Vec<(String, Option<kiro_pool::ProfileUsage>)> = Vec::new();
        for name in &names {
            eprintln!("fetching usage for {}...", name);
            results.push((name.clone(), fetch_profile_usage(pool_dir, name)));
        }
        with_state(pool_dir, |s| {
            for (name, u) in &results {
                if let Some(p) = s.profiles.get_mut(name) {
                    p.last_usage = u.clone();
                }
            }
            Ok(())
        })?;
    }
    let s = read_state(pool_dir)?;
    let now = Utc::now();

    if json {
        let rows: Vec<serde_json::Value> = s
            .order
            .iter()
            .map(|name| {
                let p = s.profiles.get(name).cloned().unwrap_or_default();
                let status = profile_status(&p, now, cfg.zombie_minutes);
                let kind = p
                    .kind
                    .as_deref()
                    .or_else(|| profile_license(pool_dir, name))
                    .unwrap_or("-");
                serde_json::json!({
                    "name": name,
                    "kind": kind,
                    "status": status,
                    "in_use_since": p.in_use_since,
                    "in_use_count": p.in_use_count,
                    "cooldown_until": p.cooldown_until,
                    "pick_count": p.pick_count,
                    "cooldown_count": p.cooldown_count,
                    "access_ttl_secs": token_ttl_secs(pool_dir, name),
                    "last_login_secs_ago": profile_mtime(pool_dir, name)
                        .map(|t| (now - t).num_seconds()),
                    "usage": p.last_usage.as_ref().map(|u| serde_json::json!({
                        "used_percent": u.used_percent,
                        "credits_used": u.credits_used,
                        "credits_total": u.credits_total,
                        "plan": u.plan,
                        "resets_at": u.resets_at,
                    })),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "profiles": rows }))?
        );
        return Ok(ExitCode::SUCCESS);
    }

    if s.order.is_empty() {
        println!("pool is empty. run: kiro-pool login <name>");
        return Ok(ExitCode::SUCCESS);
    }
    println!(
        "{:<12} {:<8} {:<9} {:<10} {:<8} {:<10} {:<10}",
        "NAME", "TYPE", "STATUS", "COOLDOWN", "ACCESS", "LAST_LOGIN", "USAGE"
    );
    for name in &s.order {
        let p = s.profiles.get(name).cloned().unwrap_or_default();
        let status = profile_status(&p, now, cfg.zombie_minutes);
        let cd_str = match p.cooldown_until {
            Some(t) if t > now => format_rel(t - now),
            _ => "-".into(),
        };
        let kind = p
            .kind
            .as_deref()
            .or_else(|| profile_license(pool_dir, name))
            .unwrap_or("-");
        let tok = token_ttl(pool_dir, name).unwrap_or_else(|| "-".into());
        let last = profile_mtime(pool_dir, name)
            .map(|t| format_rel_past(now - t))
            .unwrap_or_else(|| "-".into());
        let usage_str = p
            .last_usage
            .as_ref()
            .map(|u| format!("{:.0}%", u.used_percent))
            .unwrap_or_else(|| "-".into());
        println!(
            "{:<12} {:<8} {:<9} {:<10} {:<8} {:<10} {:<10}",
            name, kind, status, cd_str, tok, last, usage_str
        );
        if let Some(dt) = profile_mtime(pool_dir, name) {
            let days = (now - dt).num_days();
            if days >= 80 {
                println!(
                    "  !  {}: login {}d ago, refresh_token may expire (90d typical) — run `kiro-pool login {}`",
                    name, days, name
                );
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn profile_status(p: &Profile, now: DateTime<Utc>, zombie_minutes: i64) -> &'static str {
    if let Some(u) = p.in_use_since {
        if (now - u) < chrono::Duration::minutes(zombie_minutes) {
            "in_use"
        } else {
            "zombie"
        }
    } else if let Some(cd) = p.cooldown_until {
        if cd > now {
            "cooldown"
        } else {
            "idle"
        }
    } else {
        "idle"
    }
}

fn format_rel(d: chrono::Duration) -> String {
    if d.num_hours() >= 1 {
        format!("{}h{}m", d.num_hours(), d.num_minutes() % 60)
    } else if d.num_minutes() >= 1 {
        format!("{}m", d.num_minutes())
    } else {
        format!("{}s", d.num_seconds().max(0))
    }
}

fn format_rel_past(d: chrono::Duration) -> String {
    if d.num_seconds() < 0 {
        return "just now".into();
    }
    if d.num_days() >= 1 {
        return format!("{}d", d.num_days());
    }
    if d.num_hours() >= 1 {
        return format!("{}h", d.num_hours());
    }
    if d.num_minutes() >= 1 {
        return format!("{}m", d.num_minutes());
    }
    format!("{}s", d.num_seconds())
}

fn cmd_pick(pool_dir: &Path, json: bool, dry_run: bool) -> Result<ExitCode> {
    let res = with_state(pool_dir, |s| Ok(pick(s, pool_dir, dry_run)))?;
    match res {
        Ok(p) => {
            if json {
                let out = serde_json::json!({
                    "name": p.name,
                    "home": p.home,
                    "dry_run": dry_run,
                });
                println!("{}", out);
            } else {
                println!("{}", p.name);
                println!("{}", p.home.display());
                if dry_run {
                    eprintln!("(dry-run: no state written)");
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Err(e) => {
            eprintln!("{e}");
            Ok(ExitCode::from(2))
        }
    }
}

fn cmd_release(
    pool_dir: &Path,
    cfg: &Config,
    name: &str,
    cooldown: Option<&str>,
    error: bool,
) -> Result<ExitCode> {
    let dur = if let Some(d) = cooldown {
        Some(parse_duration(d)?)
    } else if error {
        Some(chrono::Duration::minutes(cfg.default_error_cooldown_min))
    } else {
        None
    };
    with_state(pool_dir, |s| {
        let p = s
            .profiles
            .get_mut(name)
            .ok_or_else(|| anyhow!("no such profile: {name}"))?;
        p.in_use_since = None;
        p.in_use_count = 0;
        if let Some(d) = dur {
            p.cooldown_until = Some(Utc::now() + d);
            p.cooldown_count = p.cooldown_count.saturating_add(1);
        }
        Ok(())
    })?;
    Ok(ExitCode::SUCCESS)
}

fn cmd_clear_cooldown(pool_dir: &Path, name: Option<&str>, all: bool) -> Result<ExitCode> {
    if !all && name.is_none() {
        return Err(anyhow!("need <name> or --all"));
    }
    with_state(pool_dir, |s| {
        if all {
            for p in s.profiles.values_mut() {
                p.cooldown_until = None;
            }
        } else if let Some(n) = name {
            let p = s
                .profiles
                .get_mut(n)
                .ok_or_else(|| anyhow!("no such profile: {n}"))?;
            p.cooldown_until = None;
        }
        Ok(())
    })?;
    if all {
        println!("cleared cooldown on all profiles");
    } else {
        println!("cleared cooldown on {}", name.unwrap());
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_remove(pool_dir: &Path, name: &str, purge: bool) -> Result<ExitCode> {
    with_state(pool_dir, |s| {
        if !s.order.iter().any(|n| n == name) && !s.profiles.contains_key(name) {
            return Err(anyhow!("no such profile: {name}"));
        }
        s.order.retain(|n| n != name);
        s.profiles.remove(name);
        Ok(())
    })?;
    if purge {
        let home = profile_home(pool_dir, name);
        if home.exists() {
            if io::stdin().is_terminal() {
                eprint!("purge {}? [y/N] ", home.display());
                io::stderr().flush().ok();
                let mut line = String::new();
                io::stdin().read_line(&mut line)?;
                if !matches!(line.trim(), "y" | "Y" | "yes") {
                    eprintln!("skipped purge");
                    return Ok(ExitCode::SUCCESS);
                }
            }
            fs::remove_dir_all(&home).with_context(|| format!("rm -rf {}", home.display()))?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_logout(pool_dir: &Path, name: &str) -> Result<ExitCode> {
    let s = read_state(pool_dir)?;
    if !s.profiles.contains_key(name) {
        return Err(anyhow!("no such profile: {name}"));
    }
    // 刪除 sqlite
    let db = profile_sqlite(pool_dir, name);
    if db.exists() {
        fs::remove_file(&db).with_context(|| format!("rm {}", db.display()))?;
        // 清 WAL/SHM/journal
        for ext in ["sqlite3-wal", "sqlite3-shm", "sqlite3-journal"] {
            let p = db.with_extension(ext);
            let _ = fs::remove_file(&p);
        }
    }
    // macOS: 清 per-profile keychain
    #[cfg(target_os = "macos")]
    {
        let home = profile_home(pool_dir, name);
        let kc = home.join("Library/Keychains/login.keychain-db");
        if kc.exists() {
            let _ = Command::new("security")
                .args(["delete-keychain"])
                .arg(&kc)
                .status();
        }
    }
    println!(
        "{}: logged out (auth data cleared, removed from pool)",
        name
    );
    with_state(pool_dir, |s| {
        s.order.retain(|n| n != name);
        s.profiles.remove(name);
        Ok(())
    })?;
    Ok(ExitCode::SUCCESS)
}

fn cmd_stats(pool_dir: &Path, json: bool) -> Result<ExitCode> {
    let s = read_state(pool_dir)?;
    if json {
        let rows: Vec<serde_json::Value> = s
            .order
            .iter()
            .map(|name| {
                let p = s.profiles.get(name).cloned().unwrap_or_default();
                serde_json::json!({
                    "name": name,
                    "pick_count": p.pick_count,
                    "cooldown_count": p.cooldown_count,
                    "usage": p.last_usage.as_ref().map(|u| serde_json::json!({
                        "used_percent": u.used_percent,
                        "credits_used": u.credits_used,
                        "credits_total": u.credits_total,
                    })),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "profiles": rows }))?
        );
        return Ok(ExitCode::SUCCESS);
    }
    if s.order.is_empty() {
        println!("pool is empty");
        return Ok(ExitCode::SUCCESS);
    }
    println!(
        "{:<12} {:<8} {:<10} {:<10}",
        "NAME", "PICKS", "COOLDOWNS", "USAGE"
    );
    for name in &s.order {
        let p = s.profiles.get(name).cloned().unwrap_or_default();
        let usage_str = p
            .last_usage
            .as_ref()
            .map(|u| {
                format!(
                    "{:.1}/{:.1} ({:.0}%)",
                    u.credits_used, u.credits_total, u.used_percent
                )
            })
            .unwrap_or_else(|| "-".into());
        println!(
            "{:<12} {:<8} {:<10} {:<10}",
            name, p.pick_count, p.cooldown_count, usage_str
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_usage(
    pool_dir: &Path,
    json: bool,
    update_state: bool,
    only: Option<&str>,
) -> Result<ExitCode> {
    let s = read_state(pool_dir)?;
    let names: Vec<&String> = if let Some(n) = only {
        if !s.order.iter().any(|x| x == n) {
            return Err(anyhow!("no such profile: {n}"));
        }
        s.order.iter().filter(|x| x.as_str() == n).collect()
    } else {
        s.order.iter().collect()
    };

    let mut results: Vec<(String, Option<kiro_pool::ProfileUsage>)> = Vec::new();
    for name in &names {
        eprintln!("fetching usage for {}...", name);
        let u = fetch_profile_usage(pool_dir, name);
        results.push((name.to_string(), u));
    }

    if json {
        let rows: Vec<serde_json::Value> = results
            .iter()
            .map(|(name, u)| match u {
                Some(u) => serde_json::json!({
                    "name": name,
                    "credits_used": u.credits_used,
                    "credits_total": u.credits_total,
                    "used_percent": u.used_percent,
                    "plan": u.plan,
                    "resets_at": u.resets_at,
                }),
                None => serde_json::json!({ "name": name, "error": "fetch failed" }),
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "usage": rows }))?
        );
    } else {
        println!(
            "{:<12} {:<8} {:<20} {:<10}",
            "NAME", "PLAN", "CREDITS", "RESETS"
        );
        for (name, u) in &results {
            match u {
                Some(u) => {
                    let credits = format!(
                        "{:.1} / {:.1} ({:.1}%)",
                        u.credits_used, u.credits_total, u.used_percent
                    );
                    println!(
                        "{:<12} {:<8} {:<20} {:<10}",
                        name,
                        u.plan.as_deref().unwrap_or("-"),
                        credits,
                        u.resets_at.as_deref().unwrap_or("-"),
                    );
                }
                None => {
                    println!("{:<12} {:<8} {:<20} {:<10}", name, "-", "fetch failed", "-");
                }
            }
        }
    }

    if update_state {
        with_state(pool_dir, |s| {
            for (name, u) in &results {
                if let Some(p) = s.profiles.get_mut(name) {
                    p.last_usage = u.clone();
                }
            }
            Ok(())
        })?;
        eprintln!("state.json updated with usage data");
    }

    Ok(ExitCode::SUCCESS)
}

fn cmd_doctor(pool_dir: &Path, only: Option<&str>) -> Result<ExitCode> {
    let real_home = std::env::var("HOME").context("HOME not set")?;
    let real_home = PathBuf::from(real_home);
    let mut any_fail = false;

    macro_rules! check {
        ($ok:expr, $label:expr) => {{
            let ok = $ok;
            println!("{} {}", if ok { "[ OK ]" } else { "[FAIL]" }, $label);
            if !ok {
                any_fail = true;
            }
        }};
        ($ok:expr, $label:expr, warn) => {{
            let ok = $ok;
            println!("{} {}", if ok { "[ OK ]" } else { "[WARN]" }, $label);
        }};
    }

    println!("== host ==");
    let kiro_data = real_home.join(kiro_pool::kiro_data_relpath());
    check!(
        kiro_data.exists(),
        format!("kiro-cli data dir exists: {}", kiro_data.display())
    );
    check!(
        real_home.join(".local/bin/kiro-cli").exists(),
        "~/.local/bin/kiro-cli exists"
    );
    check!(
        real_home.join(".local/bin/kiro-cli-chat").exists(),
        "~/.local/bin/kiro-cli-chat exists",
        warn
    );
    check!(
        real_home.join(".kiro").exists(),
        "~/.kiro/ (agent config) exists",
        warn
    );
    check!(which("kiro-cli").is_some(), "kiro-cli on PATH");

    // macOS: 檢查 user keychain search list 是否被 pool_dir keychain 污染
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("security")
            .args(["list-keychains", "-d", "user"])
            .output()
            .ok();
        if let Some(out) = output {
            let pool_str = pool_dir.to_string_lossy().to_string();
            let polluted: Vec<String> = String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter_map(|l| {
                    let t = l.trim();
                    let t = t.strip_prefix('"')?.strip_suffix('"')?;
                    Some(t.to_string())
                })
                .filter(|e| e.starts_with(&pool_str))
                .collect();
            if polluted.is_empty() {
                check!(true, "user keychain search list clean (no pool entries)");
            } else {
                println!(
                    "[FAIL] user keychain search list polluted ({} pool entries) — 直接运行 kiro-cli 会串台读 profile token",
                    polluted.len()
                );
                for p in &polluted {
                    println!("         polluted: {p}");
                }
                println!("         fix: kiro-pool fix-keychain");
                any_fail = true;
            }
        }
    }

    let s = read_state(pool_dir)?;
    let names: Vec<&String> = if let Some(n) = only {
        if !s.order.iter().any(|x| x == n) {
            return Err(anyhow!("no such profile: {n}"));
        }
        s.order.iter().filter(|x| x.as_str() == n).collect()
    } else {
        s.order.iter().collect()
    };

    for name in names {
        println!("\n== profile {} ==", name);
        let home = profile_home(pool_dir, name);
        check!(home.exists(), format!("home dir: {}", home.display()));
        let app = home.join(kiro_pool::kiro_data_relpath());
        check!(app.exists(), format!("kiro-cli app dir: {}", app.display()));
        check!(
            profile_sqlite(pool_dir, name).exists(),
            "data.sqlite3 present"
        );
        // symlink 目标解析检查
        for sub in ["bun", "tui.js"] {
            let p = app.join(sub);
            check!(p.exists(), format!("{} resolves", sub), warn);
        }
        let sib = home.join(".local/bin/kiro-cli");
        check!(sib.exists(), ".local/bin/kiro-cli symlink resolves");
        #[cfg(target_os = "macos")]
        check!(
            home.join("Library/Keychains/login.keychain-db").exists(),
            "per-profile keychain exists"
        );
        // sqlite 可打开
        let db_ok = rusqlite::Connection::open_with_flags(
            profile_sqlite(pool_dir, name),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .is_ok();
        check!(db_ok, "sqlite is readable");
    }

    if any_fail {
        Ok(ExitCode::from(1))
    } else {
        Ok(ExitCode::SUCCESS)
    }
}

fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let p = dir.join(bin);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

fn profile_mtime(pool_dir: &Path, name: &str) -> Option<DateTime<Utc>> {
    let md = fs::metadata(profile_sqlite(pool_dir, name)).ok()?;
    let modified = md.modified().ok()?;
    Some(DateTime::<Utc>::from(modified))
}

fn token_ttl(pool_dir: &Path, name: &str) -> Option<String> {
    let secs = token_ttl_secs(pool_dir, name)?;
    if secs <= 0 {
        Some("expired".into())
    } else {
        Some(format_rel(chrono::Duration::seconds(secs)))
    }
}

fn token_ttl_secs(pool_dir: &Path, name: &str) -> Option<i64> {
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
    let exp_str = v.get("expires_at").and_then(|x| x.as_str())?;
    let exp: DateTime<Utc> = DateTime::parse_from_rfc3339(exp_str)
        .ok()?
        .with_timezone(&Utc);
    Some((exp - Utc::now()).num_seconds())
}

#[allow(dead_code)]
fn _touch_rotate(pool_dir: &Path, cfg: &Config) {
    // 给以后在合适 hook 里调用的占位；当前 wrap 在 cooldown 写入后已 rotate
    let _ = rotate_logs(&pool_dir.join("logs"), cfg.log_keep);
}
