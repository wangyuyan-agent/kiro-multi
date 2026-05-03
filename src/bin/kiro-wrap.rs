//! kiro-wrap: pick → exec kiro-cli → release / cooldown。

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use kiro_pool::{
    config::Config,
    fetch_profile_usage,
    pick::{pick, Picked},
    profile_home, resolve_pool_dir, rotate_logs,
    state::{read_state, with_state},
    State, STDERR_RING_CAP,
};
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::iterator::Signals;
use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{IsTerminal, Read, Write};
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration as StdDuration, Instant};

fn main() -> ExitCode {
    match run() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("kiro-wrap: {e:#}");
            ExitCode::from(1)
        }
    }
}

type Ring = Arc<Mutex<VecDeque<u8>>>;

fn new_ring() -> Ring {
    Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_RING_CAP)))
}

/// 读取 src → 写 dst + 拷入 ring。dst 可以是 stderr / stdout。
fn spawn_tee<R: Read + Send + 'static, W: Write + Send + 'static>(
    src: R,
    mut dst: W,
    ring: Ring,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut r = src;
        let mut buf = [0u8; 4096];
        loop {
            match r.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let _ = dst.write_all(&buf[..n]);
                    let mut g = ring.lock().unwrap();
                    for &b in &buf[..n] {
                        if g.len() >= STDERR_RING_CAP {
                            g.pop_front();
                        }
                        g.push_back(b);
                    }
                }
                Err(_) => break,
            }
        }
    })
}

fn ensure_home() {
    if std::env::var("HOME").is_ok() {
        return;
    }
    // 嘗試從 libc::getpwuid 取
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if !pw.is_null() {
            let dir = std::ffi::CStr::from_ptr((*pw).pw_dir);
            if let Ok(s) = dir.to_str() {
                if !s.is_empty() {
                    std::env::set_var("HOME", s);
                    return;
                }
            }
        }
    }
    eprintln!("kiro-wrap: HOME not set — 請在調用方 env 中設置 HOME");
    std::process::exit(1);
}

fn configure_child_command(
    cmd: &mut Command,
    args: &[String],
    effective_home: &Path,
    real_home: &str,
) {
    cmd.args(args)
        .env("HOME", effective_home)
        .env("KIRO_REAL_HOME", real_home)
        .env("KIRO_PROFILE_HOME", effective_home);
}

fn maybe_refresh_usage_before_pick(pool_dir: &Path, cfg: &Config) {
    if !cfg.usage_preflight_enabled {
        return;
    }
    if let Err(e) = refresh_usage_before_pick(pool_dir, cfg) {
        eprintln!("kiro-wrap: usage preflight skipped: {e:#}");
    }
}

fn refresh_usage_before_pick(pool_dir: &Path, cfg: &Config) -> Result<()> {
    let state = read_state(pool_dir)?;
    let mut names = usage_preflight_names(&state, Utc::now(), cfg.usage_preflight_ttl_secs);
    if names.is_empty() {
        return Ok(());
    }

    let Some(lock) = acquire_usage_preflight_lock(pool_dir, cfg.usage_preflight_lock_timeout_ms)?
    else {
        eprintln!("kiro-wrap: usage preflight already running; using cached usage");
        return Ok(());
    };

    let state = read_state(pool_dir)?;
    names = usage_preflight_names(&state, Utc::now(), cfg.usage_preflight_ttl_secs);
    if names.is_empty() {
        let _ = lock.unlock();
        return Ok(());
    }

    eprintln!(
        "kiro-wrap: usage preflight refreshing {} stale idle profile(s)",
        names.len()
    );
    let mut fetched = Vec::new();
    for name in names {
        match fetch_profile_usage(pool_dir, &name) {
            Some(usage) => fetched.push((name, usage)),
            None => eprintln!("kiro-wrap: usage preflight failed for {name}; keeping cached usage"),
        }
    }

    if !fetched.is_empty() {
        with_state(pool_dir, |s| {
            for (name, usage) in &fetched {
                if let Some(p) = s.profiles.get_mut(name) {
                    p.last_usage = Some(usage.clone());
                }
            }
            Ok(())
        })?;
    }

    let _ = lock.unlock();
    Ok(())
}

fn usage_preflight_names(state: &State, now: DateTime<Utc>, ttl_secs: u64) -> Vec<String> {
    let ttl = chrono::Duration::seconds(i64::try_from(ttl_secs).unwrap_or(i64::MAX));
    state
        .order
        .iter()
        .filter(|name| {
            let Some(profile) = state.profiles.get(*name) else {
                return false;
            };
            if profile.in_use_since.is_some() {
                return false;
            }
            if profile.cooldown_until.is_some_and(|cd| cd > now) {
                return false;
            }
            match profile
                .last_usage
                .as_ref()
                .and_then(|usage| usage.updated_at)
            {
                None => true,
                Some(updated_at) => ttl_secs == 0 || now.signed_duration_since(updated_at) >= ttl,
            }
        })
        .cloned()
        .collect()
}

fn acquire_usage_preflight_lock(pool_dir: &Path, timeout_ms: u64) -> Result<Option<File>> {
    fs::create_dir_all(pool_dir)?;
    let lock_path = pool_dir.join("usage-refresh.lock");
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("open {}", lock_path.display()))?;
    let deadline = Instant::now() + StdDuration::from_millis(timeout_ms);

    loop {
        match FileExt::try_lock_exclusive(&lock) {
            Ok(()) => return Ok(Some(lock)),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                let now = Instant::now();
                if now >= deadline {
                    return Ok(None);
                }
                let remaining = deadline.saturating_duration_since(now);
                std::thread::sleep(std::cmp::min(StdDuration::from_millis(100), remaining));
            }
            Err(e) => return Err(anyhow::Error::from(e).context("try_lock_exclusive")),
        }
    }
}

fn run() -> Result<ExitCode> {
    ensure_home();
    let real_home = std::env::var("HOME").context("HOME not set after ensure_home")?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let pool_dir = resolve_pool_dir(None)?;
    fs::create_dir_all(&pool_dir)?;
    // 清理上次 wrap 被 SIGKILL / OOM / 斷電留下的 stale shared profile 目錄
    let _ = kiro_pool::cleanup_stale_shared_profiles(&pool_dir);
    let cfg = Config::load(&pool_dir)?;
    if cfg.flock_timeout_ms != kiro_pool::DEFAULT_FLOCK_TIMEOUT_MS {
        std::env::set_var(
            "KIRO_POOL_FLOCK_TIMEOUT_MS",
            cfg.flock_timeout_ms.to_string(),
        );
    }

    let forced_profile = std::env::var("KIRO_POOL_PROFILE").ok();
    if forced_profile.is_none() {
        maybe_refresh_usage_before_pick(&pool_dir, &cfg);
    }

    let pick_res = if let Some(forced) = forced_profile {
        // 指定 profile，跳過輪轉，但仍標 in_use_since
        with_state(&pool_dir, |s| {
            let p = s
                .profiles
                .get(&forced)
                .ok_or_else(|| anyhow::anyhow!("KIRO_POOL_PROFILE={forced}: no such profile"))?;
            let now = Utc::now();
            if let Some(cd) = p.cooldown_until {
                if cd > now {
                    return Err(anyhow::anyhow!(
                        "KIRO_POOL_PROFILE={forced}: in cooldown until {cd}"
                    ));
                }
            }
            let entry = s.profiles.get_mut(&forced).unwrap();
            entry.in_use_since = Some(now);
            entry.in_use_count = entry.in_use_count.saturating_add(1);
            entry.cooldown_until = None;
            entry.pick_count = entry.pick_count.saturating_add(1);
            let kind = entry.kind.clone().unwrap_or_else(|| "free".to_string());
            Ok(Ok(Picked {
                name: forced.clone(),
                home: profile_home(&pool_dir, &forced),
                kind,
                shared: false,
            }))
        })?
    } else {
        with_state(&pool_dir, |s| Ok(pick(s, &pool_dir, false)))?
    };
    let picked = match pick_res {
        Ok(p) => p,
        Err(e) => {
            eprintln!("kiro-wrap: {e}, try later");
            return Ok(ExitCode::from(1));
        }
    };
    if picked.shared {
        eprintln!(
            "kiro-wrap: [shared] reusing profile {} (already in use)",
            picked.name
        );
    }
    let name = picked.name.clone();

    // shared pick → 創建臨時 profile 目錄（認證共享，sqlite/pid 獨立）
    let (effective_home, shared_dir) = if picked.shared {
        let dir = kiro_pool::create_shared_profile(&pool_dir, &name, std::process::id())
            .context("create shared profile")?;
        (dir.clone(), Some(dir))
    } else {
        (picked.home.clone(), None)
    };

    let setup = if shared_dir.is_none() {
        kiro_pool::ensure_keychain(&effective_home)
            .and_then(|_| kiro_pool::ensure_shared_assets(&effective_home))
            .and_then(|_| kiro_pool::ensure_sibling_binaries(&effective_home))
            .and_then(|_| kiro_pool::ensure_kiro_config(&effective_home))
    } else {
        // shared profile: keychain 仍需 unlock（per-process），其餘已在 create_shared_profile 設置
        kiro_pool::ensure_keychain(&effective_home)
    };
    if let Err(e) = setup {
        eprintln!("kiro-wrap: setup profile {}: {e:#}", name);
        if let Some(dir) = &shared_dir {
            let _ = fs::remove_dir_all(dir);
        }
        let _ = with_state(&pool_dir, |s| {
            if let Some(p) = s.profiles.get_mut(&name) {
                p.in_use_count = p.in_use_count.saturating_sub(1);
                if p.in_use_count == 0 {
                    p.in_use_since = None;
                }
            }
            Ok(())
        });
        return Ok(ExitCode::from(1));
    }

    // 若用户没指定子命令、也没要 help/version，就补一个 `chat` —— 裸跑 `kiro-wrap`
    // 直接進入交互會話，對齊舊行為（kiro-cli 空參數會顯示菜單而非進 chat）。
    let args = inject_default_subcmd(args);
    // 第一个非 flag 的 token 视为 kiro-cli 子命令（acp / chat / agent / ...）。
    let subcmd: Option<&str> = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .map(|s| s.as_str());

    // 按 picked.kind 注入 --model（config.toml 的 [tier_model]）。用户显式传 --model 时不覆盖。
    // 注：kiro-cli 的 chat/acp 子命令只定義了 `--model`，沒有 `-m` 短名，所以這裡只檢 `--model`。
    // 若將來 kiro-cli 加 `-m`，需要同步更新這個檢測。
    let user_has_model = args
        .iter()
        .any(|a| a == "--model" || a.starts_with("--model="));
    let mut args = args.clone();
    if !user_has_model {
        if let Some(model) = cfg.tier_model.get(&picked.kind) {
            args.push("--model".to_string());
            args.push(model.clone());
        }
    }

    // stdout：
    //  - TTY（交互 chat）inherit
    //  - 非 TTY + 子命令 acp：直接 inherit，不走 tee（ACP 是 JSON-RPC，cooldown regex 无意义，
    //    且多一道 pipe + LineWriter + 线程调度会在 openab 握手窗口里引入 race）
    //  - 其余非 TTY（pipeline）：pipe + tee，保留 cooldown 侦测
    //  - KIRO_WRAP_NO_STDOUT_TEE=1 强制 inherit
    let no_tee_env = std::env::var("KIRO_WRAP_NO_STDOUT_TEE")
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false);
    let tee_stdout = !no_tee_env && subcmd != Some("acp") && !std::io::stdout().is_terminal();

    let mut cmd = Command::new("kiro-cli");
    configure_child_command(&mut cmd, &args, &effective_home, &real_home);
    cmd.stdin(Stdio::inherit()).stderr(Stdio::piped());
    if tee_stdout {
        cmd.stdout(Stdio::piped());
    } else {
        cmd.stdout(Stdio::inherit());
    }
    let mut child = cmd.spawn().context("spawn kiro-cli")?;

    let child_pid = child.id() as i32;

    let stderr_ring = new_ring();
    let stderr_reader = child.stderr.take().expect("piped stderr");
    let stderr_t = spawn_tee(stderr_reader, std::io::stderr(), Arc::clone(&stderr_ring));

    let stdout_ring = new_ring();
    let stdout_t = if tee_stdout {
        let r = child.stdout.take().expect("piped stdout");
        Some(spawn_tee(r, std::io::stdout(), Arc::clone(&stdout_ring)))
    } else {
        None
    };

    let mut signals = Signals::new([SIGINT, SIGTERM, SIGHUP])?;
    thread::spawn(move || {
        for sig in signals.forever() {
            unsafe {
                libc::kill(child_pid, sig);
            }
        }
    });

    let status = child.wait().context("wait kiro-cli")?;
    stderr_t.join().ok();
    if let Some(t) = stdout_t {
        t.join().ok();
    }

    let exit_code: u8 = match status.code() {
        Some(c) => (c & 0xff) as u8,
        None => {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                128u8.saturating_add(status.signal().unwrap_or(0) as u8)
            }
            #[cfg(not(unix))]
            {
                1
            }
        }
    };

    let stderr_tail = String::from_utf8_lossy(
        &stderr_ring
            .lock()
            .unwrap()
            .iter()
            .copied()
            .collect::<Vec<u8>>(),
    )
    .into_owned();
    let stdout_tail = if tee_stdout {
        Some(
            String::from_utf8_lossy(
                &stdout_ring
                    .lock()
                    .unwrap()
                    .iter()
                    .copied()
                    .collect::<Vec<u8>>(),
            )
            .into_owned(),
        )
    } else {
        None
    };

    let re = regex::Regex::new(&cfg.cooldown_regex).context("compile cooldown regex")?;
    // 檢測 quota exhausted 信號。只匹配明確跟 quota/credits 相關的字樣，
    // **不** 匹配一般的 `-32603` / "Internal error" — 那些 JSON-RPC 通用錯誤碼幾乎任何
    // 失敗都會帶，用來判 quota 會把所有偶發錯誤誤標為永久耗盡。
    let quota_re = regex::Regex::new(
        r"(?i)(quota\s*(?:exceeded|exhausted)|usage\s*limit\s*(?:reached|exceeded)|credits?\s*(?:exhausted|depleted)|out\s+of\s+credits|no\s+credits\s+(?:left|remain|remaining))",
    )
    .context("compile quota regex")?;
    let combined_for_match = match &stdout_tail {
        Some(so) => format!("{stderr_tail}\n{so}"),
        None => stderr_tail.clone(),
    };
    let quota_exhausted = quota_re.is_match(&combined_for_match);

    let matched = re
        .find(&stderr_tail)
        .map(|m| ("stderr", m.as_str().to_string()))
        .or_else(|| {
            stdout_tail
                .as_deref()
                .and_then(|s| re.find(s).map(|m| ("stdout", m.as_str().to_string())))
        });
    let triggered = matched.is_some();

    let cooldown = if triggered {
        let combined_tail = match &stdout_tail {
            Some(so) => format!("---stderr---\n{stderr_tail}\n---stdout---\n{so}"),
            None => stderr_tail.clone(),
        };
        let matched_str = matched.as_ref().map(|(src, m)| format!("[{src}] {m}"));
        let _ = log_cooldown(
            &pool_dir,
            &name,
            std::process::id(),
            exit_code,
            &combined_tail,
            matched_str.as_deref(),
        );
        let _ = rotate_logs(&pool_dir.join("logs"), cfg.log_keep);
        Some(chrono::Duration::minutes(cfg.default_error_cooldown_min))
    } else {
        None
    };
    with_state(&pool_dir, |s| {
        if let Some(p) = s.profiles.get_mut(&name) {
            p.in_use_count = p.in_use_count.saturating_sub(1);
            if p.in_use_count == 0 {
                p.in_use_since = None;
            }
            if let Some(d) = cooldown {
                p.cooldown_until = Some(Utc::now() + d);
                p.cooldown_count = p.cooldown_count.saturating_add(1);
            }
            // quota 耗盡：標記 last_usage 為 100%，後續 pick 跳過。
            // 若之前沒查過 usage（resets_at 為空），填一個下月 1 號的默認值 —
            // Kiro 訂閱按月重置，這個默認不完美但能保證 pick 在月初自動解禁，
            // 避免被動學習打的標記永久鎖住 profile。
            if quota_exhausted {
                let mut usage = p.last_usage.take().unwrap_or_default();
                usage.used_percent = 100.0;
                usage.updated_at = Some(Utc::now());
                if usage.resets_at.is_none() {
                    usage.resets_at = Some(default_resets_at());
                }
                p.last_usage = Some(usage);
            }
        }
        Ok(())
    })?;

    // 清理 shared profile 臨時目錄
    if let Some(dir) = shared_dir {
        let _ = fs::remove_dir_all(&dir);
    }

    Ok(ExitCode::from(exit_code))
}

/// 若 `args` 里既没有子命令、也没有 help/version flag，则在最前面补 `chat`。
/// 这样裸跑 `kiro-wrap` 等价于 `kiro-wrap chat`，对齐用户对旧版本的预期。
/// 任何已显式带子命令（acp / agent / settings / ...）或顶层 help/version 的调用都不动。
///
/// 判断规则：kiro-cli 的 subcommand 永远是 args[0]（不会在 flag 之后），
/// 所以只看 args[0] 是否以 `-` 开头即可——这样 `--agent foo` 里的 `foo`
/// 就不会被误判成 subcommand。help/version flag 出现在任意位置都视为 help 调用。
fn inject_default_subcmd(args: Vec<String>) -> Vec<String> {
    let has_help_or_version = args.iter().any(|a| {
        matches!(
            a.as_str(),
            "-h" | "--help" | "--help-all" | "-V" | "--version"
        )
    });
    let has_subcmd = args.first().map(|a| !a.starts_with('-')).unwrap_or(false);
    if has_subcmd || has_help_or_version {
        return args;
    }
    let mut out = Vec::with_capacity(args.len() + 1);
    out.push("chat".to_string());
    out.extend(args);
    out
}

/// 下月 1 號的 YYYY-MM-DD 字符串，給被動學習 quota 耗盡時當默認 resets_at。
fn default_resets_at() -> String {
    use chrono::Datelike;
    let now = Utc::now().date_naive();
    let (y, m) = if now.month() == 12 {
        (now.year() + 1, 1u32)
    } else {
        (now.year(), now.month() + 1)
    };
    chrono::NaiveDate::from_ymd_opt(y, m, 1)
        .expect("valid next-month date")
        .format("%Y-%m-%d")
        .to_string()
}

fn log_cooldown(
    pool_dir: &Path,
    name: &str,
    pid: u32,
    exit_code: u8,
    tail: &str,
    matched: Option<&str>,
) -> Result<()> {
    let logs = pool_dir.join("logs");
    fs::create_dir_all(&logs)?;
    let epoch = Utc::now().timestamp();
    let path = logs.join(format!("{name}-{pid}-{epoch}.log"));
    let mut f = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)?;
    writeln!(f, "profile: {name}")?;
    writeln!(f, "wrap_pid: {pid}")?;
    writeln!(f, "exit_code: {exit_code}")?;
    writeln!(f, "time: {}", Utc::now().to_rfc3339())?;
    writeln!(f, "matched: {}", matched.unwrap_or("-"))?;
    writeln!(f, "---tail---")?;
    f.write_all(tail.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{configure_child_command, inject_default_subcmd, usage_preflight_names};
    use chrono::{Duration, Utc};
    use kiro_pool::{Profile, ProfileUsage, State};
    use std::{collections::BTreeMap, ffi::OsString, path::PathBuf, process::Command};

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    fn usage_at(updated_at: chrono::DateTime<Utc>) -> ProfileUsage {
        ProfileUsage {
            updated_at: Some(updated_at),
            ..ProfileUsage::default()
        }
    }

    #[test]
    fn empty_args_get_chat_prepended() {
        assert_eq!(inject_default_subcmd(v(&[])), v(&["chat"]));
    }

    #[test]
    fn explicit_subcmd_is_unchanged() {
        assert_eq!(inject_default_subcmd(v(&["chat"])), v(&["chat"]));
        assert_eq!(inject_default_subcmd(v(&["acp"])), v(&["acp"]));
        assert_eq!(
            inject_default_subcmd(v(&["agent", "list"])),
            v(&["agent", "list"])
        );
    }

    #[test]
    fn flags_only_with_help_or_version_are_unchanged() {
        for f in ["-h", "--help", "--help-all", "-V", "--version"] {
            assert_eq!(inject_default_subcmd(v(&[f])), v(&[f]), "flag={f}");
        }
    }

    #[test]
    fn flags_only_without_help_get_chat_prepended() {
        // 例如 `kiro-wrap --agent foo` → `kiro-cli chat --agent foo`
        assert_eq!(
            inject_default_subcmd(v(&["--agent", "foo"])),
            v(&["chat", "--agent", "foo"])
        );
        assert_eq!(
            inject_default_subcmd(v(&["--model=claude-sonnet-4-5"])),
            v(&["chat", "--model=claude-sonnet-4-5"])
        );
    }

    #[test]
    fn subcmd_with_help_keeps_subcmd_help() {
        // `kiro-wrap chat --help` → 不动，让 kiro-cli chat 自己显示 help
        assert_eq!(
            inject_default_subcmd(v(&["chat", "--help"])),
            v(&["chat", "--help"])
        );
    }

    #[test]
    fn child_env_exposes_real_and_profile_home() {
        let profile_home = PathBuf::from("/tmp/kiro-profile");
        let real_home = "/tmp/real-home";
        let args = v(&["acp", "--trust-all-tools"]);
        let mut cmd = Command::new("kiro-cli");

        configure_child_command(&mut cmd, &args, &profile_home, real_home);

        let envs: BTreeMap<OsString, Option<OsString>> = cmd
            .get_envs()
            .map(|(k, v)| (k.to_os_string(), v.map(|v| v.to_os_string())))
            .collect();
        assert_eq!(
            envs.get(&OsString::from("HOME")).and_then(|v| v.as_ref()),
            Some(&profile_home.clone().into_os_string())
        );
        assert_eq!(
            envs.get(&OsString::from("KIRO_PROFILE_HOME"))
                .and_then(|v| v.as_ref()),
            Some(&profile_home.into_os_string())
        );
        assert_eq!(
            envs.get(&OsString::from("KIRO_REAL_HOME"))
                .and_then(|v| v.as_ref()),
            Some(&OsString::from(real_home))
        );
        assert_eq!(
            cmd.get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            args
        );
    }

    #[test]
    fn usage_preflight_refreshes_only_stale_idle_profiles() {
        let now = Utc::now();
        let mut state = State {
            order: vec![
                "fresh".to_string(),
                "stale".to_string(),
                "missing".to_string(),
                "busy".to_string(),
                "cooldown".to_string(),
            ],
            ..State::default()
        };
        state.profiles.insert(
            "fresh".to_string(),
            Profile {
                last_usage: Some(usage_at(now - Duration::seconds(30))),
                ..Profile::default()
            },
        );
        state.profiles.insert(
            "stale".to_string(),
            Profile {
                last_usage: Some(usage_at(now - Duration::minutes(10))),
                ..Profile::default()
            },
        );
        state
            .profiles
            .insert("missing".to_string(), Profile::default());
        state.profiles.insert(
            "busy".to_string(),
            Profile {
                in_use_since: Some(now),
                last_usage: Some(usage_at(now - Duration::minutes(10))),
                ..Profile::default()
            },
        );
        state.profiles.insert(
            "cooldown".to_string(),
            Profile {
                cooldown_until: Some(now + Duration::minutes(5)),
                last_usage: Some(usage_at(now - Duration::minutes(10))),
                ..Profile::default()
            },
        );

        let names = usage_preflight_names(&state, now, 300);

        assert_eq!(names, vec!["stale".to_string(), "missing".to_string()]);
    }

    #[test]
    fn usage_preflight_ttl_zero_refreshes_any_idle_profile() {
        let now = Utc::now();
        let mut state = State {
            order: vec!["fresh".to_string()],
            ..State::default()
        };
        state.profiles.insert(
            "fresh".to_string(),
            Profile {
                last_usage: Some(usage_at(now)),
                ..Profile::default()
            },
        );

        assert_eq!(usage_preflight_names(&state, now, 0), vec!["fresh"]);
    }
}
