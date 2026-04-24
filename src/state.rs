//! state.json 的读写与 flock 封装。

use crate::{State, SCHEMA_VERSION};
use anyhow::{anyhow, Context, Result};
use fs2::FileExt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn state_path(pool_dir: &Path) -> PathBuf {
    pool_dir.join("state.json")
}

fn lock_path(pool_dir: &Path) -> PathBuf {
    pool_dir.join("state.json.lock")
}

fn ensure_pool_dir(pool_dir: &Path) -> Result<()> {
    fs::create_dir_all(pool_dir)
        .with_context(|| format!("create pool dir {}", pool_dir.display()))?;
    Ok(())
}

fn open_lock(pool_dir: &Path) -> Result<File> {
    ensure_pool_dir(pool_dir)?;
    OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(lock_path(pool_dir))
        .with_context(|| "open state lockfile")
}

/// flock 超时 ms。优先读 `KIRO_POOL_FLOCK_TIMEOUT_MS` 环境变量，否则用默认常量。
/// （不直接依赖 Config 是因为 state 模块要避免循环依赖 / 读 config 本身可能再 lock）
fn flock_timeout_ms() -> u64 {
    std::env::var("KIRO_POOL_FLOCK_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(crate::DEFAULT_FLOCK_TIMEOUT_MS)
}

fn lock_exclusive_with_timeout(lock: &File) -> Result<()> {
    let deadline = Instant::now() + Duration::from_millis(flock_timeout_ms());
    loop {
        // UFCS 明确走 fs2 trait method，避免和 stdlib 1.89+ 的同名 inherent 方法冲突。
        match FileExt::try_lock_exclusive(lock) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(anyhow!(
                        "flock timeout after {}ms — 另一个进程持有 state 锁；排查 zombie wrap 或手动删 state.json.lock",
                        flock_timeout_ms()
                    ));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(anyhow::Error::from(e).context("try_lock_exclusive")),
        }
    }
}

fn lock_shared_with_timeout(lock: &File) -> Result<()> {
    let deadline = Instant::now() + Duration::from_millis(flock_timeout_ms());
    loop {
        match FileExt::try_lock_shared(lock) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(anyhow!("flock shared timeout"));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(anyhow::Error::from(e).context("try_lock_shared")),
        }
    }
}

fn load_unlocked(pool_dir: &Path) -> Result<State> {
    let p = state_path(pool_dir);
    if !p.exists() {
        return Ok(State::default());
    }
    let mut s = String::new();
    File::open(&p)
        .with_context(|| format!("open {}", p.display()))?
        .read_to_string(&mut s)?;
    if s.trim().is_empty() {
        return Ok(State::default());
    }
    let st: State = serde_json::from_str(&s).with_context(|| format!("parse {}", p.display()))?;
    if st.schema_version > SCHEMA_VERSION {
        return Err(anyhow!(
            "state.json schema_version={} 高于 binary 支持的 {}；升级 binary 或手动 downgrade",
            st.schema_version,
            SCHEMA_VERSION
        ));
    }
    Ok(st)
}

fn save_atomic(pool_dir: &Path, state: &State) -> Result<()> {
    let p = state_path(pool_dir);
    let tmp = p.with_extension("json.tmp");
    let data = serde_json::to_vec_pretty(state)?;
    {
        let mut f = File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(&data)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, &p).with_context(|| format!("rename {} -> {}", tmp.display(), p.display()))?;
    Ok(())
}

/// 排他锁 + 回调修改 state + 原子落盘。落盘前自动 bump 到当前 SCHEMA_VERSION。
pub fn with_state<F, T>(pool_dir: &Path, f: F) -> Result<T>
where
    F: FnOnce(&mut State) -> Result<T>,
{
    let lock = open_lock(pool_dir)?;
    lock_exclusive_with_timeout(&lock)?;
    let result = (|| {
        let mut state = load_unlocked(pool_dir)?;
        let out = f(&mut state)?;
        state.schema_version = SCHEMA_VERSION;
        save_atomic(pool_dir, &state)?;
        Ok::<_, anyhow::Error>(out)
    })();
    let _ = lock.unlock();
    result
}

/// 共享锁的只读快照。
pub fn read_state(pool_dir: &Path) -> Result<State> {
    let lock = open_lock(pool_dir)?;
    lock_shared_with_timeout(&lock)?;
    let res = load_unlocked(pool_dir);
    let _ = lock.unlock();
    res
}
