# kiro-multi

> 🌐 **Language**: **English** · [简体中文](README.zh-CN.md)
>
> 🤖 **AI agents**: jump to [For AI agents](#for-ai-agents) for a deployment runbook tailored to coding agents.
>
> ⚠️ **Disclaimer**: This project is for managing **legally owned** Kiro CLI accounts (e.g. personal Builder ID + edu plan). Do **not** use it to circumvent AWS Terms of Service or for commercial resale. Use at your own risk; the author assumes no liability.

Multi-account toolkit for [Kiro CLI](https://aws.amazon.com/kiro/), shipped as two binaries:

- **`kiro-pool`** — control plane: `login` / `logout` / `list` / `usage` / `tag` / `remove`. Manages account-pool lifecycle and state (`state.json`, protected by flock).
- **`kiro-wrap`** — data plane: each session atomically picks one profile, rewrites `HOME`, then `exec`s `kiro-cli`. The profile is sticky for the child's lifetime; on exit the wrapper inspects the stderr tail for rate-limit hits and passively learns quota exhaustion.

Each profile is an independent `HOME` — `kiro-cli`'s sqlite / keychain / history are isolated by directory and never leak across accounts.

## Requirements

- **Kiro CLI ≥ 2.1** (recommended). v0.2.0 of kiro-multi assumes device-flow login and the `toolSearch.enabled` setting key — both shipped in Kiro CLI 2.1. If you must stay on a CLI older than 2.1, pin `kiro-multi = "0.1"`.
- **Rust toolchain** to build (`cargo install --path .`).

## Platform support

- **macOS** (Apple Silicon / Intel): per-profile keychain (`security create-keychain`) + `Library/Application Support/kiro-cli/` paths. The `security` calls run with an isolated `HOME`, so your real user keychain search list is **never** polluted.
- **Linux** (Ubuntu / Debian / RHEL — anywhere Kiro CLI runs): keychain code is a no-op (Kiro CLI uses a file-based fallback on Linux); data lives under `.local/share/kiro-cli/` (XDG). Everything else is identical. Kiro CLI 2.1+ added official RHEL TUI support; the pool layer doesn't care which distro you pick.

## Layout

macOS:

```
~/.kiro-pool/
├── config.toml              # optional: override constants (cooldown_regex / zombie_minutes / ...)
├── state.json               # rotation state (with schema_version)
├── state.json.lock          # flock
├── logs/<name>-<pid>-<ts>.log
└── profiles/<name>/
    ├── Library/Keychains/login.keychain-db        # per-profile keychain
    ├── .kiro/               -> ~/.kiro            # shared agent config (agents/skills/settings/memory.md)
    └── Library/Application Support/kiro-cli/
        ├── data.sqlite3                           # per-profile (independent)
        ├── history                                # per-profile
        ├── knowledge_bases/                       # per-profile
        ├── bun           -> ~/Library/.../bun     # shared, read-only
        ├── tui.js        -> ~/Library/.../tui.js  # shared, read-only
        └── shell/        -> ~/Library/.../shell/  # shared, read-only
```

Under shared mode a temporary directory `profiles/<name>__shared_<pid>/` is created and auto-removed when the session ends.

Linux differs in two places: no `Library/Keychains/`, and the kiro-cli data dir is `~/.kiro-pool/profiles/<name>/.local/share/kiro-cli/`.

## Isolation guarantees

- **Writes**: sqlite, keychain, history, knowledge_bases all live under `~/.kiro-pool/profiles/<name>/`, fully independent.
- **Reads**: `bun` / `tui.js` / `shell/` / `~/.local/bin/kiro-cli{,-chat,-term}` are reused via symlink — no copy, no write.
- Your real `~/Library/Application Support/kiro-cli/data.sqlite3`, keychain entries, and `knowledge_bases/` are **never modified** by the pool. Verify with `ls -la` mtime if you're paranoid.

## Install

```bash
cargo install --path .
# Installs ~/.cargo/bin/{kiro-pool,kiro-wrap}
```

## Login

```bash
kiro-pool login a                          # default --tier free, opens interactive login menu
kiro-pool login b --tier student           # edu mailbox student plan
kiro-pool login c --tier pro+              # tag personal Pro+ / Power as appropriate
```

The Kiro CLI login menu pops up:

```
? Select login method ›
❯ Use for Free with Builder ID
  Use for Free with Google or GitHub
  Use with Pro license
```

**Remote login on a VPS / SSH / container** (Kiro CLI ≥ 2.1, device flow): kiro-cli prints a one-time code plus a `https://app.kiro.dev/account/device?user_code=...` URL. Open the URL in any browser (your laptop, your phone, anywhere), confirm the code, done. No port forwarding, no SSH tunnel, no callback URL relay. kiro-pool just inherits stdio so the prompt lands directly in your terminal.

> Older Kiro CLI (< 2.1) used a `localhost:3128` OAuth callback. kiro-multi v0.1.x had a relay shim for that flow; **v0.2.0 dropped it** because device flow is now the default and the shim only added 60 seconds of dead waiting. If you need to log in with a CLI older than 2.1, downgrade to kiro-multi v0.1.x.

**Organization subscription** (with an IAM Identity Center start URL):

```bash
kiro-pool login a --tier pro --identity-provider https://<idc>.awsapps.com/start
```

`--tier` does not affect the login flow; it only labels the TYPE column in `kiro-pool list` and influences the pick policy.

## Logout

```bash
kiro-pool logout a    # clears auth (sqlite + keychain) and removes from the pool
```

After logout the profile no longer appears in `list` / `stats`. Re-`login` to restore it.

## Status

```bash
kiro-pool list
# NAME     TYPE  STATUS    COOLDOWN  ACCESS  LAST_LOGIN  USAGE
# a        free  idle      -         59m     7s          45%
# b        pro   cooldown  3m        42m     2h          -

kiro-pool list --refresh-usage   # query usage live and persist to state (slow)
```

- **TYPE**: subscription tier label, one of `free / student / pro / pro+ / power`.
- **ACCESS**: time-to-live of the access token (~1h, kiro-cli refreshes it automatically; informational).
- **LAST_LOGIN**: time since the last sqlite write. ≥ 80 days emits an extra warning line.
- **USAGE**: last queried credit usage percentage (run `usage --update-state` or `list --refresh-usage` first).

## Usage queries

```bash
kiro-pool usage                    # query credit usage per profile (spawns kiro-cli /usage)
kiro-pool usage --json             # JSON output
kiro-pool usage --update-state     # persist results to state.json; pick will skip 100% drained profiles
kiro-pool usage student_1          # one profile only
```

**Quota exhaustion handled automatically**:

- **Passive learning**: when a session ends, kiro-wrap inspects the stderr tail for quota signals (`-32603 Internal error` etc.) and marks the profile at 100%. Subsequent picks skip it. One mistake is enough — no preflight needed.
- **Lazy preflight**: before an automatic pick, kiro-wrap refreshes stale usage for idle, non-cooldown profiles. The default TTL is 5 minutes and a separate `usage-refresh.lock` prevents concurrent refresh storms.
- **Auto-unfreeze on reset day**: at pick time, if `resets_at` has passed, the stale 100% mark is ignored automatically.
- **Cold-start protection**: lazy preflight covers a fresh `state.json` on the first automatic pick. `ExecStartPre=/path/to/kiro-pool usage --update-state` is still useful for systemd deployments when you want to pay that latency at service start instead of on the first user request.

## Pick / release (rarely needed by hand)

```bash
kiro-pool pick --json              # {"name":"A","home":"/Users/.../profiles/A"}
kiro-pool pick --dry-run --json    # preview only, doesn't touch state / pick_count
kiro-pool release A
kiro-pool release A --cooldown 10m
kiro-pool release A --error        # apply config or default 5-minute cooldown
kiro-pool clear-cooldown A         # clear one
kiro-pool clear-cooldown --all     # clear all
```

## Other operational commands

```bash
kiro-pool list --json              # machine-readable, with usage / pick_count / cooldown_count / access_ttl_secs
kiro-pool stats                    # per-profile cumulative PICKS / COOLDOWNS / USAGE
kiro-pool stats --json
kiro-pool doctor                   # health check: pool dir / kiro-cli / ~/.kiro / each profile
kiro-pool doctor <name>            # check one profile only
kiro-pool fix-keychain             # macOS: scrub stale per-profile keychains from the user search list
kiro-pool fix-keychain --dry-run   # show what would be removed without touching anything
kiro-pool completion zsh > ~/.zfunc/_kiro-pool   # shell completion (bash/zsh/fish/elvish/powershell)
```

### `~/.kiro-pool/config.toml`

All fields are optional and fall back to built-in defaults:

```toml
zombie_minutes            = 30        # treat in_use_since older than this as a zombie at pick time
default_error_cooldown_min = 5        # default cooldown applied by release --error and regex hits
cooldown_regex            = "(?i)(concurrent|too many|retry in \\d|throttl|rate[\\s-]?limit|try again later|quota|exceeded)"
log_keep                  = 50        # keep the most recent N cooldown tail logs in logs/
flock_timeout_ms          = 5000      # flock acquire timeout per command
usage_preflight_enabled   = true      # kiro-wrap refreshes stale idle usage before automatic pick
usage_preflight_ttl_secs  = 300       # refresh an idle profile only when cached usage is older than this
usage_preflight_lock_timeout_ms = 60000 # wait for another preflight refresh before using cached usage

# tier → kiro-cli default model. wrap injects `--model <X>` automatically based on the picked profile's
# tier, since settings/cli.json is shared across the pool — per-profile override has to be a CLI flag.
# If the user passes --model explicitly we don't override.
[tier_model]
free    = "claude-sonnet-4.5"
student = "claude-sonnet-4.5"
pro     = "claude-opus-4.6"
"pro+"  = "claude-opus-4.6"
power   = "claude-opus-4.6"
```

Changes take effect on the next invocation — no daemon to restart.

### kiro-wrap environment

Incoming env switches:

| env | effect |
|---|---|
| `KIRO_POOL_DIR` | override the default `~/.kiro-pool` |
| `KIRO_POOL_PROFILE` | force a specific profile, skip rotation (still tracked as in_use / released) |
| `KIRO_WRAP_NO_STDOUT_TEE=1` | force stdout `inherit` instead of tee+ring. ACP already takes this path automatically; flip this if a non-ACP pipeline gets stuck on handshake |

Exported to the child `kiro-cli` process:

| env | effect |
|---|---|
| `KIRO_REAL_HOME` | the caller's real HOME before kiro-wrap rewrites it |
| `KIRO_PROFILE_HOME` | the effective Kiro profile HOME assigned to this session, including `__shared_<pid>` homes |

## kiro-wrap

A transparent shim around `kiro-cli` that lifts the "which account?" decision out of the caller.

**Contract**:

- Bare `kiro-wrap` (no subcommand) defaults to `chat` so `kiro-wrap` alone enters an interactive session, matching the legacy behaviour.
- All other CLI args are passed through to `kiro-cli` verbatim. `kiro-wrap` does not consume any flags of its own — pool selection is via env, not flags.
- stdin: `inherit`. stderr: always teed (64 KiB ring buffer drives cooldown detection). stdout: `inherit` on a TTY (preserves interactive chat); on non-TTY (openab / ACP / pipelines) also teed into the ring buffer so kiro-cli can't sneak a rate-limit message past us via stdout.
- Exit code is propagated; killed-by-signal returns `128 + signum`.
- SIGINT / SIGTERM / SIGHUP are forwarded to the child (not swallowed).
- env: `KIRO_POOL_DIR` overrides the default `~/.kiro-pool`; `HOME` is rewritten to `<pool>/profiles/<picked>` for the child; `KIRO_REAL_HOME` and `KIRO_PROFILE_HOME` expose both sides of that rewrite to the agent.
- **HOME defence**: if `HOME` is unset at startup, kiro-wrap tries `getpwuid` first; if that also fails, it exits with a clear error rather than silently failing inside setup (a common openab footgun).

**Flow**:

1. If no `KIRO_POOL_PROFILE` is forced, refresh stale usage for idle, non-cooldown profiles according to `usage_preflight_ttl_secs`.
2. Atomically pick the lowest-tier available profile, mark `in_use_since` (flock-protected).
3. Materialize per-profile keychain + runtime symlinks (`bun` / `tui.js` / `shell/` / `~/.local/bin/kiro-cli{,-chat,-term}`).
4. `spawn HOME=<effective-profile-home> KIRO_REAL_HOME=<caller-home> KIRO_PROFILE_HOME=<effective-profile-home> kiro-cli <args...>`.
5. On child exit:
   - If the stderr tail (and stdout tail in non-TTY mode) matches `cooldown_regex` → set cooldown and dump the tail to `logs/<name>-<pid>-<ts>.log`. Logs auto-rotate at `log_keep`.
   - If a quota-exhaustion signal (`-32603` / `Internal error`) is detected → also mark `last_usage = 100%` so future picks skip the profile.
   - Otherwise just clear `in_use_since`.
6. When cooldown fires, inspect `~/.kiro-pool/logs/` for the actual AWS message and tweak `cooldown_regex` in `config.toml` if needed.

### Day-to-day use

```bash
alias kiro='kiro-wrap'
kiro                                       # bare kiro-wrap → kiro-cli chat
kiro chat "hello"

# Force a specific profile (skip rotation)
KIRO_POOL_PROFILE=student_1 kiro-wrap chat "hi"

# Switch to a different pool
KIRO_POOL_DIR=/data/my-pool kiro-wrap chat "hi"
```

### External integrations (openab / ACP / automation)

Typical caller: [openab](https://github.com/openabdev/openab) spawns one long-lived `kiro-cli acp` process per Discord thread, speaking JSON-RPC.

```toml
# openab per-thread command
command     = "/root/.cargo/bin/kiro-wrap"
args        = ["acp", "--trust-all-tools"]
working_dir = "/root"
env         = { KIRO_POOL_DIR = "/root/.kiro-pool", HOME = "/root" }
```

> **⚠️ Paths must be adjusted to your environment, not copy-pasted.** `command` / `working_dir` / env paths depend on the deploy user and cargo install location:
>
> | scenario | command | working_dir | HOME |
> |---|---|---|---|
> | root user | `/root/.cargo/bin/kiro-wrap` | `/root` | `/root` |
> | ubuntu user | `/home/ubuntu/.cargo/bin/kiro-wrap` | `/home/ubuntu` | `/home/ubuntu` |
> | custom user | `/<home>/.cargo/bin/kiro-wrap` | `/<home>` | `/<home>` |
>
> Generic formula: `$(eval echo ~<user>)/.cargo/bin/kiro-wrap`. Confirm with `which kiro-wrap` before deploying.

Field notes:

- **`command`**: absolute path to `kiro-wrap`. openab spawns child processes without going through the shell PATH.
- **`args`**: passed straight to `kiro-cli`. `"acp"` enables ACP JSON-RPC mode; `"--trust-all-tools"` skips MCP tool-call confirmation (no human present in ACP mode — without this it just hangs).
- **`working_dir`**: openab's child process working dir. Set it to the user's home so kiro-cli resolves relative paths sensibly.
- **`env.HOME`**: **required**. kiro-wrap needs HOME to locate the real kiro-cli data dir and pool dir; missing HOME = hard error on startup.
- **`env.KIRO_POOL_DIR`**: pool directory, defaults to `~/.kiro-pool`. Set explicitly to remove ambiguity.

> **Note**: the caller's env must include `HOME`, or kiro-wrap can't bootstrap the profile environment.

Coding-agent tool credentials:

`kiro-wrap` intentionally gives Kiro CLI a profile HOME. That means tools launched by the agent also see `$HOME` as the profile directory, so user-level credentials in the real HOME are not found automatically. This is expected for tools such as `gh`, `git`, `ssh`, `aws`, `docker`, and npm-like CLIs.

kiro-wrap exports the original home as `KIRO_REAL_HOME`. Agents should opt into it when a command needs user-level credentials:

```bash
HOME="$KIRO_REAL_HOME" gh auth status
GH_CONFIG_DIR="$KIRO_REAL_HOME/.config/gh" gh auth status
HOME="$KIRO_REAL_HOME" git config --global user.email
```

Recommended persistent instruction for agents:

```md
When running under kiro-multi, HOME is a Kiro profile home. The original user home is available as KIRO_REAL_HOME, and the current Kiro profile home is KIRO_PROFILE_HOME. For external developer tools that need user-level credentials or config, such as gh, git, ssh, aws, docker, npm, or cloud CLIs, prefer running them with HOME="$KIRO_REAL_HOME" or the tool-specific config env.
```

Operational notes:

- **Sticky lifetime**: while a thread (and therefore the wrap process) is alive, the profile stays `in_use_since`. Profile is released when the wrap exits. One wrap = one profile; do not try to switch mid-session.
- **Concurrency = multiple wraps**: flock keeps pick/release serialized — two threads will not race on the same profile. When the pool is saturated (every profile in_use, none idle), pick falls back to **shared mode** and reuses the profile with the lowest usage rather than failing. Only when *all* profiles are in cooldown does pick return `all profiles busy or in cooldown`.
- **Concurrency under shared mode**: a single profile may be shared by multiple wraps (refcounted); AWS may still trigger `TooManyConcurrent` on its side, in which case the wrap takes the normal cooldown path. Steady high concurrency = add more accounts.
- **Zombie reaping**: `kill -9` or a power loss can leave `in_use_since` stuck; after `zombie_minutes` (default 30) the next pick treats it as available. Tune via `zombie_minutes` in `config.toml`.
- **Don't add flags to wrap**: every flag is consumed by kiro-cli and there is no way to intercept. Pool config is via env; policy lives in code.

VPS deploy checklist:

1. `kiro-pool login <name> --tier <...>` for each account.
2. Run `kiro-cli chat` once under your real HOME so it bootstraps `bun` / `tui.js` into the kiro-cli data dir (macOS: `~/Library/Application Support/kiro-cli/`; Linux: `~/.local/share/kiro-cli/`). Without this, the pool's symlinks have nothing to point at.
3. `kiro-pool doctor`; `kiro-pool list` and check TYPE / LAST_LOGIN.
4. Recommended: `kiro-pool usage --update-state` to populate usage up front. If you skip it, kiro-wrap lazy preflight still runs before automatic picks.
5. Run openab under systemd; the env must contain `KIRO_POOL_DIR` and `HOME`; the systemd `User=` must own the pool dir (flock permissions).

## Remove

```bash
kiro-pool remove A --purge   # interactive confirmation; non-TTY just deletes
```

## Pick policy

**Tier-step fallback + per-tier round-robin**.

1. Scan tiers `free → student → pro → pro+ → power` from low to high.
2. Within a tier, use a per-tier cursor for round-robin, skipping busy / cooldown / zombie / exhausted / logged-out profiles.
3. Move up a tier only when the current one is fully unavailable.

The intent is **burn cheap stuff first, save the expensive accounts**: cooldown is only 5 minutes, so even if free-tier accounts get hammered it's fine; pro / Identity Center accounts are harder to replace and should only be used when the cheaper tiers are wiped out.

Profiles without an explicit tag are treated as `free` (consumed first).

**Shared fallback (when the pool is saturated)**:

When every profile is `in_use`, pick no longer returns `AllBusy`. Instead it walks the same tier order and picks the in-use profile with the lowest usage and lowest concurrency count. In this mode:

- Profiles use refcounted in-use tracking (`in_use_count`); `in_use_since` clears when the count hits zero.
- kiro-wrap creates a temporary `{name}__shared_{pid}` directory: keychain symlinked (auth shared), sqlite copied (independent writes) — avoids kiro-cli's PID-file mutex.
- The temporary directory is auto-removed when the session ends.
- stderr emits `[shared] reusing <name>` so you know what happened.

This means the pool never refuses service just because concurrency exceeds the profile count — at the cost of shared profiles hitting rate-limits faster.

## Other tradeoffs

- No scoring / weighting: tier-step already encodes "save the high tier", weights add nothing.
- No randomization: deterministic round-robin is easier to debug ("who got picked last?").
- No cron: kiro-cli refreshes its own access_token before expiry.
- No always-on quota probing loop: kiro-wrap only refreshes stale idle usage before automatic picks, plus passive learning (mark 100% on wall-hit) and auto-unfreeze at month boundary.
- One AWS account → one profile on one host; **don't rsync `profiles/` across machines**.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| Discord bot reports "⚠️ Connection Lost" | kiro-wrap exited at startup because `HOME` was unset | add `HOME = "/root"` (or the deploy user's home) to openab config.toml's env |
| `gh auth status` says "not logged in" inside kiro-wrap but works in your normal shell | `$HOME` is intentionally the Kiro profile HOME, so `gh` looks under `<profile>/.config/gh` instead of the real user's config | run `HOME="$KIRO_REAL_HOME" gh ...` or `GH_CONFIG_DIR="$KIRO_REAL_HOME/.config/gh" gh ...`; add the agent instruction above |
| "all profiles busy or in cooldown" | every profile is in cooldown (shared fallback already handles pure in-use saturation) | wait out the cooldown (default 5 min) or add more accounts |
| "⚠️ Internal Error (code: -32603)" | profile's quota is exhausted; AWS rejected the request | run `kiro-pool usage --update-state`; the next wrap auto-learns and skips this profile |
| `list` USAGE column is all `-` | usage was never queried | run `kiro-pool usage --update-state` once, or `kiro-pool list --refresh-usage` |
| TYPE shows `free` after login but profile is actually student | `--tier` label was wrong | `kiro-pool tag <name> student` |
| kiro-cli login hangs (VPS) | n/a in v0.2.0+ — Kiro CLI ≥ 2.1 uses device flow, no callback listener | open the printed `app.kiro.dev/account/device?user_code=...` URL in any browser to confirm |
| "flock timeout" | another process is holding the state lock | check for zombie wrap processes; `kill` them or wait for `zombie_minutes` to elapse |
| Profile still shown 100% after the reset day | `resets_at` in state.json hasn't actually passed yet | normally pick auto-checks `resets_at` and unfreezes; if the date is wrong, refresh with `kiro-pool usage --update-state` |
| macOS `security` keeps polluting the user keychain search list | older versions of this tool ran `security` without isolating HOME | `kiro-pool fix-keychain` to scrub; new builds (≥ v0.1.0) prevent it at the source |

### Full systemd example

> Example below assumes the root user. For other users, replace `/root` with the corresponding home and update `User=`.

```ini
[Unit]
Description=openab with kiro-pool
After=network.target

[Service]
Type=simple
User=root
# Optional: refresh usage at service start instead of making the first request pay preflight latency.
ExecStartPre=/root/.cargo/bin/kiro-pool usage --update-state
ExecStart=/root/.cargo/bin/openab
Environment=KIRO_POOL_DIR=/root/.kiro-pool
Environment=HOME=/root
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
```

`ExecStartPre` queries every profile's credit usage and writes it to `state.json` before openab starts (including on crash-restart). `kiro-wrap` also has lazy usage preflight before automatic picks, so this line is mainly a latency placement choice: boot-time refresh vs. first-request refresh.

> **After editing the unit file you must reload:**
> ```bash
> sudo systemctl daemon-reload
> sudo systemctl restart openab.service
> ```
> Skipping `daemon-reload` and going straight to start/restart will fail (systemd keeps the old unit cached).

### Periodic usage refresh (optional)

```bash
# crontab -e
# Refresh usage every hour to keep state.json fresh
0 * * * * /root/.cargo/bin/kiro-pool usage --update-state >/dev/null 2>&1
```

## For AI agents

> This section is a deployment / configuration runbook for **coding agents helping a user set up kiro-multi**. If you are a human, you can ignore it — the rest of the README already covers everything. If you are an agent: read this section in full before running commands. It encodes the constraints that the rest of the README leaves implicit.

### Identify the goal first

Pick exactly one of these intents based on what the user said. Each maps to a different command sequence:

| User intent | Path |
|---|---|
| Try kiro-multi locally with one account | **Path A — local single account** |
| Run multiple accounts on this workstation, manual `kiro-wrap` | **Path B — local multi-account** |
| Deploy on a VPS so an external integration (openab / cron / a chatbot) can use rotated accounts | **Path C — VPS + integration** |
| Move accounts from one machine to another | **Path D — re-login on the new host (do NOT rsync)** |

If the user's intent is ambiguous, ask **one** clarifying question. Do not assume.

### Hard constraints (do NOT violate)

1. **Never `rsync` / `cp -r` / `tar` the `~/.kiro-pool/profiles/` directory across machines.** Auth tokens are bound to the host's keychain (macOS) or filesystem ACL. Re-login on the target machine instead.
2. **Never edit `state.json` by hand.** It is flock-protected; use `kiro-pool` subcommands (`tag`, `release`, `clear-cooldown`, `remove`) instead.
3. **Never run `security` / `keychain` commands manually inside a profile dir.** All keychain provisioning goes through `kiro-pool login` and `kiro-pool fix-keychain`.
4. **Do not commit `.kiro-pool/` to any repo.** It contains auth material.
5. **Do not pass flags to `kiro-wrap` itself** — every flag is forwarded to `kiro-cli`. Use env vars for pool config: `KIRO_POOL_DIR`, `KIRO_POOL_PROFILE`, `KIRO_WRAP_NO_STDOUT_TEE`.
6. **`HOME` must be set in the calling environment** when invoking `kiro-wrap` from systemd / cron / openab. Missing `HOME` = hard error at startup. Always set it explicitly.
7. **Usage data should be fresh before real traffic.** kiro-wrap performs lazy preflight before automatic picks by default; for deployments, run `kiro-pool usage --update-state` at boot if you want refresh latency to happen before the first user request.
8. **One AWS account = one profile on one host.** Do not log the same account into two pools on different hosts simultaneously — kiro-cli's session state will diverge and one side will silently break.
9. **Inside kiro-wrap, `$HOME` is the Kiro profile HOME.** If an external tool needs the real user's global credentials or config, use `KIRO_REAL_HOME` explicitly, for example `HOME="$KIRO_REAL_HOME" gh auth status`. Do not copy broad dotfile trees into profile homes as a default fix.

### Path A — local single account

```bash
# 1. Build (only if installing from source)
cargo install --path .

# 2. Make sure real kiro-cli has run at least once (bootstraps bun / tui.js)
kiro-cli --version            # if this fails, install kiro-cli first; STOP here and tell the user

# 3. Create a profile and log in
kiro-pool login a --tier free   # or --tier student / pro / pro+ / power as appropriate

# 4. Verify
kiro-pool doctor
kiro-pool usage --update-state  # recommended cold-start refresh; wrap also has lazy preflight
kiro-pool list                  # confirm STATUS=idle, USAGE shown

# 5. Use it
kiro-wrap                       # bare wrap defaults to chat
```

If `kiro-pool doctor` reports `[FAIL]`, **stop and report the failure to the user**. Do not auto-fix unless the failure is `user keychain search list polluted` — for that one, run `kiro-pool fix-keychain` (macOS only).

### Path B — local multi-account

Same as Path A, but repeat step 3 for each account with distinct names. Use meaningful names matching the tier:

```bash
kiro-pool login free_1     --tier free
kiro-pool login student_1  --tier student
kiro-pool login student_2  --tier student
kiro-pool login pro_1      --tier pro
```

Then `kiro-pool list` should show all profiles with correct TYPE. Pick policy will burn `free_*` first, then `student_*`, then `pro_*`. The user can `alias kiro='kiro-wrap'` for daily use.

### Path C — VPS + integration

This is the most error-prone path. The four common failure modes:

| Symptom user reports | Root cause | Verify with | Fix |
|---|---|---|---|
| "Connection Lost" / silent fail at session start | Caller's env missing `HOME` | check the systemd unit / openab config | add `HOME=/<user-home>` to env |
| `gh` / `git` / `ssh` sees a blank user config | Agent tool shell runs with the profile HOME by design | `echo "$HOME"; echo "$KIRO_REAL_HOME"` inside the agent | teach the agent to use `HOME="$KIRO_REAL_HOME" <tool>` when it needs user-level credentials |
| `kiro-cli` not found / no such file | systemd `User=` differs from cargo install user | `which kiro-wrap` as the systemd user | use absolute path `/<home>/.cargo/bin/kiro-wrap` everywhere; never rely on PATH |
| First request after restart errors with `-32603` | Usage preflight was disabled, failed, or cached data was still stale | check `state.json` for empty/stale `last_usage` and service stderr for `usage preflight` | keep lazy preflight enabled, or add `ExecStartPre=/<home>/.cargo/bin/kiro-pool usage --update-state` to systemd unit |
| `flock timeout` / pick fails | Wrong owner on `~/.kiro-pool/` | `ls -la ~/.kiro-pool/state.json` | `chown -R <systemd-user>:<group> ~/.kiro-pool/`; never run `kiro-pool` as root if openab runs as a non-root user (or vice versa) |

Standard VPS deploy checklist (do these in order, do not skip):

1. Confirm: which Linux user will run the integration? (e.g. `root`, `ubuntu`, `openab`). Call this `$U`. All paths below use `$U`'s home.
2. As `$U`, install kiro-cli first; verify with `kiro-cli --version`.
3. Run `kiro-cli chat` once and exit immediately — this bootstraps `bun` / `tui.js` into `~/.local/share/kiro-cli/`. Skip this and pool symlinks have nothing to point at.
4. As `$U`: `cargo install --path .` (or copy prebuilt binary into `~/.cargo/bin/`).
5. `kiro-pool login <name> --tier <tier>` for each account. On a headless VPS with Google/GitHub: kiro-cli ≥ 2.1 prints a device-flow URL (`app.kiro.dev/account/device?user_code=...`) — open it in any browser (laptop, phone) and confirm. No SSH tunnel needed.
6. `kiro-pool doctor` — must be all `[OK]`.
7. `kiro-pool usage --update-state` — recommended cold-start refresh; lazy preflight also runs before automatic picks.
8. Wire up the integration. For openab + systemd, see the [Full systemd example](#full-systemd-example) section above. **Always include both `KIRO_POOL_DIR` and `HOME` in `Environment=`**.
9. After editing the systemd unit, **always** `sudo systemctl daemon-reload && sudo systemctl restart <unit>`. Skipping `daemon-reload` will silently use the old unit.
10. Tail logs after starting: `journalctl -u <unit> -f` until you see a successful first request.

### Path D — moving accounts to a new host

**Do NOT rsync `~/.kiro-pool/profiles/`** — credentials will not work on the target.

Correct flow:

1. On the new host, run Path A or B from scratch.
2. `kiro-pool login` each account again (same names if you want).
3. The destination box gets fresh tokens, bound to its own keychain / filesystem.

The only thing safe to copy is `~/.kiro-pool/config.toml` (your tuning of cooldown_regex, tier_model, etc.).

### Verification commands (run these often)

After any change, run the appropriate subset:

| After… | Run |
|---|---|
| install / build | `kiro-pool --version && kiro-wrap --version` |
| login | `kiro-pool doctor <name>` then `kiro-pool list` |
| config edit | `kiro-pool list --json` (will fail to parse if config.toml is malformed) |
| any state change you made | `cat ~/.kiro-pool/state.json | jq .` |

If `kiro-pool doctor` flags `[FAIL] user keychain search list polluted`, run `kiro-pool fix-keychain --dry-run` first to show what will be removed, then `kiro-pool fix-keychain` to apply.

### When to ask the user instead of acting

- The user did not specify which **tier** for an account (`--tier` defaults to `free` and changes pick priority).
- The user did not specify the **deploy user** for VPS / systemd setups.
- An existing pool is present at `~/.kiro-pool/` and you would be adding to it — confirm before touching.
- `kiro-pool doctor` fails with anything other than the keychain pollution case.
- The user asked to "share accounts across machines" — push back: that's Path D, do not rsync.

### What this tool is and isn't

- **Is**: a per-`HOME` profile multiplexer that rotates between Kiro CLI accounts the user already legally owns. The tool does not bypass authentication, scrape AWS, or fake credentials.
- **Isn't**: an account creator, a credential cracker, or a way to circumvent quotas. If a user asks for that, refuse.

## License

MIT — see [LICENSE](LICENSE).
