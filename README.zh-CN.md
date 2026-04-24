# kiro-multi

> 🌐 **语言**: [English](README.md) · **简体中文**
>
> 🤖 **AI agent**：直接跳到 [给 AI agent 的部署指南](#给-ai-agent-的部署指南)，那一节是为 coding agent 写的 runbook。
>
> ⚠️ **免责声明**：本项目仅用于管理用户**合法持有**的 Kiro CLI 账号（如个人 Builder ID + edu plan），不得用于商用转售或绕过 AWS 服务条款。使用风险自担，作者不承担任何责任。

Kiro CLI 多账号工具集，两个二进制：

- **`kiro-pool`** — 控制面：login / logout / list / usage / tag / remove，管账号池生命周期和状态（flock 保护 `state.json`）。
- **`kiro-wrap`** — 数据面：每次起 session 原子挑一个 profile，改写 `HOME` 后 exec `kiro-cli`，子进程生命周期内粘滞；退出时按 stderr tail 判定是否进 rate-limit 冷却，并被动学习 quota 耗尽状态。

每个 profile 是独立的 `HOME`，`kiro-cli` 的 sqlite / keychain / history 按目录隔离，互不干扰。

## 平台支持

- **macOS**：per-profile keychain（`security create-keychain`）+ `Library/Application Support/kiro-cli/` 路径。
- **Linux**：keychain 相关是 no-op（`kiro-cli` 在 Linux 自己走文件 fallback），数据路径用 `.local/share/kiro-cli/`（XDG）。其余逻辑一致。

## 布局

macOS：

```
~/.kiro-pool/
├── config.toml              # 可选：覆盖常量（cooldown_regex / zombie_minutes / ...）
├── state.json               # 轮询状态 (含 schema_version)
├── state.json.lock          # flock
├── logs/<name>-<pid>-<ts>.log
└── profiles/<name>/
    ├── Library/Keychains/login.keychain-db        # per-profile keychain
    ├── .kiro/               -> ~/.kiro            # 共享 agent 配置（agents/skills/settings/memory.md）
    └── Library/Application Support/kiro-cli/
        ├── data.sqlite3                           # per-profile（独立）
        ├── history                                # per-profile
        ├── knowledge_bases/                       # per-profile
        ├── bun           -> ~/Library/.../bun     # 共享只读
        ├── tui.js        -> ~/Library/.../tui.js  # 共享只读
        └── shell/        -> ~/Library/.../shell/  # 共享只读
```

Shared 模式下会在 `profiles/` 下创建临时目录 `<name>__shared_<pid>/`，session 结束后自动清理。

Linux 版布局差异：没有 `Library/Keychains/`；`kiro-cli` 数据目录改成 `~/.kiro-pool/profiles/<name>/.local/share/kiro-cli/`。

## 隔离边界

- **写**：sqlite、keychain、history、knowledge_bases 全在 `~/.kiro-pool/profiles/<name>/` 下，完全独立。
- **读**：`bun` / `tui.js` / `shell/` / `~/.local/bin/kiro-cli{,-chat,-term}` 通过 symlink 复用真实安装，不复制、不写入。
- 真实 HOME 下的 `~/Library/Application Support/kiro-cli/data.sqlite3`、keychain entries、`knowledge_bases/` **永远不会被 pool 修改**。想验证就 `ls -la` 那几个文件的 mtime。

## 安装

```bash
cargo install --path .
# 会装到 ~/.cargo/bin/{kiro-pool,kiro-wrap}
```

## 登录

```bash
kiro-pool login a                          # 默认 --tier free，弹出交互式登录选单
kiro-pool login b --tier student           # edu 邮箱 student plan
kiro-pool login c --tier pro+              # 个人 Pro+ / Power 档按实际打标
```

登录时 kiro-cli 会弹出交互式选单：

```
? Select login method ›
❯ Use for Free with Builder ID
  Use for Free with Google or GitHub
  Use with Pro license
```

**VPS 远程登录（OAuth callback relay）**：选 Google/GitHub 后，kiro-cli 会在 VPS 上监听 `localhost:3128` 等待 OAuth callback。浏览器在本地完成登录后会跳转到 `http://localhost:3128/...`（页面打不开是正常的）。kiro-pool 会自动检测到 listener 并提示你把浏览器地址栏的 URL 贴回终端，自动 curl 完成回调。无需 SSH 隧道。

**组织订阅**（有 IAM Identity Center start URL）：

```bash
kiro-pool login a --tier pro --identity-provider https://<idc>.awsapps.com/start
```

`--tier` 档位不影响登录方式，只影响 `kiro-pool list` 里的 TYPE 列和 pick 策略。

## 登出

```bash
kiro-pool logout a    # 清 auth 数据（sqlite + keychain），从 pool 中移除
```

登出后 `list` / `stats` 不再显示该 profile。重新 `login` 即可恢复。

## 状态

```bash
kiro-pool list
# NAME     TYPE  STATUS    COOLDOWN  ACCESS  LAST_LOGIN  USAGE
# a        free  idle      -         59m     7s          45%
# b        pro   cooldown  3m        42m     2h          -

kiro-pool list --refresh-usage   # 实时查询各 profile 的 usage 并写入 state（慢）
```

- **TYPE**: 订阅档位标签，取值 `free / student / pro / pro+ / power`。
- **ACCESS**: access token 还剩多久（约 1h TTL，kiro-cli 自动刷新，仅供参考）。
- **LAST_LOGIN**: sqlite 最近一次写入至今。≥ 80 天会额外打一行警告。
- **USAGE**: 上次查询的 credits 使用百分比（需先跑 `usage --update-state` 或 `list --refresh-usage`）。

## Usage 查询

```bash
kiro-pool usage                    # 查询各 profile 的 credits 用量（spawn kiro-cli /usage）
kiro-pool usage --json             # JSON 格式输出
kiro-pool usage --update-state     # 查完写入 state.json，pick 时自动跳过 100% 用尽的 profile
kiro-pool usage student_1          # 只查某个 profile
```

**Quota 耗尽自动处理**：

- **被动学习**：kiro-wrap 在 session 结束时如果检测到 quota 相关错误（`-32603 Internal error` 等），会自动把该 profile 标记为 100% 用尽，后续 pick 跳过。第一次撞墙即学会，无需预先查询。
- **月初自动解禁**：pick 时如果 `resets_at` 日期已过（月初 quota 重置），自动忽略旧的 100% 标记，允许重新 pick。
- **冷启动必须**：新装或清空 state.json 后，**必须**跑一次 `kiro-pool usage --update-state`，否则首次 pick 到已耗尽的 profile 会直接报错。openab systemd 应加 `ExecStartPre=/path/to/kiro-pool usage --update-state`。

## 挑选和释放（一般不用手动）

```bash
kiro-pool pick --json              # {"name":"A","home":"/Users/.../profiles/A"}
kiro-pool pick --dry-run --json    # 只看会选谁，不改 state / 不 bump pick_count
kiro-pool release A
kiro-pool release A --cooldown 10m
kiro-pool release A --error        # 按 config 或默认 5 分钟冷却
kiro-pool clear-cooldown A         # 清一个
kiro-pool clear-cooldown --all     # 全清
```

## 其它运维命令

```bash
kiro-pool list --json              # 机器可读，含 usage / pick_count / cooldown_count / access_ttl_secs
kiro-pool stats                    # 打印每个 profile 的 PICKS / COOLDOWNS / USAGE 累计
kiro-pool stats --json
kiro-pool doctor                   # 自检：pool 目录 / kiro-cli / ~/.kiro / 每个 profile
kiro-pool doctor <name>            # 只检查一个 profile
kiro-pool completion zsh > ~/.zfunc/_kiro-pool   # shell 补全（bash/zsh/fish/elvish/powershell）
```

### 配置 `~/.kiro-pool/config.toml`

所有字段可选，缺省回落到内置常量：

```toml
zombie_minutes            = 30        # pick 时认定 in_use_since 超多久为 zombie，自动回收
default_error_cooldown_min = 5        # release --error / 命中 regex 时的默认冷却时长
cooldown_regex            = "(?i)(concurrent|too many|retry in \\d|throttl|rate[\\s-]?limit|try again later|quota|exceeded)"
log_keep                  = 50        # logs/ 下保留最新 N 个 cooldown tail
flock_timeout_ms          = 5000      # flock 拿不到锁时的轮询上限（单次命令等待时间）

# tier → kiro-cli 默认 model 映射。wrap 会在 pick 到 profile 后，按 kind 自动注入
# `--model <X>`，让不同档位账号用不同的 default model（settings/cli.json 是全池共享的，
# 所以需要 CLI flag 做 per-profile 覆盖）。用户显式传 --model 时不覆盖。
[tier_model]
free    = "claude-sonnet-4.5"
student = "claude-sonnet-4.5"
pro     = "claude-opus-4.6"
"pro+"  = "claude-opus-4.6"
power   = "claude-opus-4.6"
```

改完直接生效，不需要重启什么进程（每次 wrap / pool 启动都会读）。

### kiro-wrap env 开关

| env | 作用 |
|---|---|
| `KIRO_POOL_DIR` | 覆盖默认 `~/.kiro-pool` |
| `KIRO_POOL_PROFILE` | 指定使用某个 profile，跳过轮转（仍标 in_use / release） |
| `KIRO_WRAP_NO_STDOUT_TEE=1` | 强制 stdout 直接 `inherit`，不走 tee+ring。ACP 子命令已经自动走这条路径，其他 pipeline 场景遇到握手超时可手动打开 |

## kiro-wrap

把 `kiro-cli` 包一层的透明 shim，把「该用哪个账号」的决策从调用方剥离。

**Contract**：

- 所有 CLI 参数**原样透传**给 `kiro-cli`。`kiro-wrap` 本身**不认任何 flag**（包括 `--help`）——调池用 env，不是 flag。
- stdin `inherit`；stderr 永远被 tee（64 KiB 环形缓冲供 cooldown 判定）；stdout 在 TTY 下 `inherit`（保留交互 chat 原样），非 TTY（openab / ACP / pipeline）下也 tee 一份进环形缓冲，防止 kiro-cli 把 rate-limit 报错写到 stdout 而被漏判。
- 退出码透传子进程；被信号杀时返回 `128 + signum`。
- SIGINT / SIGTERM / SIGHUP 会转发给子进程（不自己吞）。
- env：`KIRO_POOL_DIR` 覆盖默认 `~/.kiro-pool`；`HOME` 会被 wrap 重写成 `<pool>/profiles/<picked>`。
- **HOME 防御**：如果启动时 `HOME` 未设，会尝试从 `getpwuid` 获取；仍然失败则打明确错误退出（避免 openab 等调用方忘记传 HOME 导致静默失败）。

**流程**：

1. 原子 pick 一个档位最低的可用 profile，标 `in_use_since`（flock 保护）
2. 补齐 per-profile keychain + runtime symlink（`bun` / `tui.js` / `shell/` / `~/.local/bin/kiro-cli{,-chat,-term}`）
3. `spawn HOME=<pool>/profiles/<name> kiro-cli <args...>`
4. 子进程退出时：
   - stderr tail（非 TTY 场景也包含 stdout tail）匹配 `cooldown_regex` → 设 cooldown 并把 tail 落到 `logs/<name>-<pid>-<ts>.log`；logs 按 `log_keep` 自动轮转
   - 检测到 quota 耗尽信号（`-32603` / `Internal error`）→ 额外标记 `last_usage = 100%`，后续 pick 跳过
   - 否则只清 `in_use_since`
5. 冷却触发时去 `~/.kiro-pool/logs/` 看具体 AWS 文案，按需改 `config.toml` 里的 `cooldown_regex`

### 日常使用

```bash
alias kiro='kiro-wrap chat'
kiro "hello"

# 指定 profile（跳过轮转）
KIRO_POOL_PROFILE=student_1 kiro-wrap chat "hi"

# 切到别的池
KIRO_POOL_DIR=/data/my-pool kiro-wrap chat "hi"
```

### 外部接入（openab / ACP / 自动化）

调用方典型场景：[openab](https://github.com/openabdev/openab) 给每个 Discord thread spawn 一个 `kiro-cli acp` 长进程说 JSON-RPC。

```toml
# openab per-thread 命令
command     = "/root/.cargo/bin/kiro-wrap"
args        = ["acp", "--trust-all-tools"]
working_dir = "/root"
env         = { KIRO_POOL_DIR = "/root/.kiro-pool", HOME = "/root" }
```

> **⚠️ 路径必须按实际环境配置，不能照搬。** `command` / `working_dir` / env 中的路径取决于你的部署用户和 cargo 安装位置：
>
> | 场景 | command | working_dir | HOME |
> |------|---------|-------------|------|
> | root 用户 | `/root/.cargo/bin/kiro-wrap` | `/root` | `/root` |
> | ubuntu 用户 | `/home/ubuntu/.cargo/bin/kiro-wrap` | `/home/ubuntu` | `/home/ubuntu` |
> | 自定义用户 | `/<home>/.cargo/bin/kiro-wrap` | `/<home>` | `/<home>` |
>
> 通用公式：`$(eval echo ~<user>)/.cargo/bin/kiro-wrap`。部署前用 `which kiro-wrap` 确认实际路径。

字段说明：

- **`command`**：`kiro-wrap` 的绝对路径。openab spawn 子进程时不走 shell PATH，必须写全路径。
- **`args`**：透传给 `kiro-cli` 的参数。`"acp"` 启动 ACP JSON-RPC 模式；`"--trust-all-tools"` 让 kiro-cli 跳过 MCP tool 调用确认（ACP 场景下无人交互，不加会卡住）。
- **`working_dir`**：openab spawn 子进程的工作目录。设为对应用户的 home 即可，确保 kiro-cli 能正常解析相对路径。
- **`env.HOME`**：**必须设置**。kiro-wrap 启动时需要 HOME 来定位真实 kiro-cli 数据目录和 pool 目录；未设则直接报错退出。
- **`env.KIRO_POOL_DIR`**：pool 目录位置，默认 `~/.kiro-pool`。显式设置避免歧义。

> **注意**：调用方 env 中必须包含 `HOME`，否则 kiro-wrap 无法初始化 profile 环境。

操作建议：

- **生命周期粘滞**：thread 活着 profile 一直被占（`in_use_since`）；thread 退出（wrap 退出）profile 才释放。一个 wrap 进程 = 一个 profile，不要试图中途切换。
- **并发 = 多个 wrap**：flock 保证 pick/release 串行，不会两个 thread 抢同一 profile。池耗光时（所有 profile 都 in_use 且无空闲）会自动 fallback 到 shared 模式复用 usage 最低的 profile，不再直接 `exit 1`。只有所有 profile 都在 cooldown 时才会报 `all profiles busy or in cooldown`。
- **Shared 模式下的并发**：同一 profile 可被多个 wrap 共享（引用计数），但 AWS 侧仍可能触发 `TooManyConcurrent`——此时 wrap 正常走 cooldown 流程。需要稳定高并发 = 加更多账号。
- **zombie 回收**：`kill -9` 或机器掉电会让 `in_use_since` 卡住；默认 30 分钟后 pick 自动视作可用。需要更激进的阈值改 `config.toml` 里的 `zombie_minutes`。
- **不要在 wrap 里加 flag**：所有 flag 都会被 kiro-cli 吃掉，拦不住。调池目录用 env，调策略改代码。

VPS 部署 checklist：

1. 每个账号 `kiro-pool login <name> --tier <...>`
2. 先在真实 HOME 跑一次 `kiro-cli chat` 让它把 `bun` / `tui.js` 初始化到 `kiro-cli` 数据目录（macOS: `~/Library/Application Support/kiro-cli/`；Linux: `~/.local/share/kiro-cli/`），否则 pool 的 symlink 无处可指
3. `kiro-pool doctor` 跑一遍；`kiro-pool list` 检查 TYPE / LAST_LOGIN
4. `kiro-pool usage --update-state` 刷一遍 usage（避免首次撞墙）
5. openab 用 systemd 起，env 必须包含 `KIRO_POOL_DIR` 和 `HOME`；跑 systemd 的 user 必须是 pool 目录的 owner（flock 权限）

## 移除

```bash
kiro-pool remove A --purge   # 带交互确认；非 TTY 会直接删
```

## 挑选策略

**阶梯回落 + 档内 round-robin**。

1. 按 `free → student → pro → pro+ → power` 从低到高扫描档位
2. 每个档内用 per-tier cursor 顺序轮转，跳过 busy / cooldown / zombie / exhausted / logged_out
3. 低档全部不可用才升档

设计目的是**贱卖先烧、贵的留手**：cooldown 5 分钟就恢复，free 耗得再快也问题不大；pro 档/Identity Center 更贵更难再拿一个，只在低档全军覆没时才动用。

没打标签的 profile 按 free 处理（最先消耗）。

**Shared fallback（池满复用）**：

当所有 profile 都处于 `in_use` 状态时，pick 不再报 `AllBusy`，而是按同样的阶梯顺序 fallback 到已占用的 profile 中 usage 最低、并发数最少的那个。此时：

- Profile 使用引用计数（`in_use_count`），归零才清 `in_use_since`。
- kiro-wrap 为 shared session 创建临时目录 `{name}__shared_{pid}`：keychain symlink 共享认证，sqlite 复制（独立写入），避免 kiro-cli 的 PID file 互斥。
- session 结束后临时目录自动清理。
- stderr 会打 `[shared] reusing <name>` 提示。

这意味着池子不会因为并发数超过 profile 数而直接拒绝服务——代价是共享 profile 可能更快触发 rate-limit。

## 其他取舍

- 没有打分 / 加权：阶梯回落已经捕获了"高档兜底"的偏好，再加权没用
- 没有随机化：档内顺序 round-robin 可预测，debug 时能看清"上次谁被选"
- 没有 cron：`kiro-cli` 自己会在 expires_at 前刷新 access_token
- 没有主动 quota 探测：被动学习（撞墙标 100%）+ 月初自动解禁已覆盖所有场景
- 一个 AWS 账号对应一台机器上一个 profile；**不要跨机 rsync `profiles/`**

## Troubleshooting

| 现象 | 原因 | 解法 |
|------|------|------|
| Discord bot 报 "⚠️ Connection Lost" | kiro-wrap 启动时 `HOME` 未设，setup 阶段立刻退出 | openab config.toml 的 env 加 `HOME = "/root"`（或对应用户的 home） |
| "all profiles busy or in cooldown" | 池中所有 profile 都在 cooldown（shared fallback 已启用，纯 in_use 不再报此错） | 等 cooldown 过期（默认 5 分钟），或加更多账号 |
| "⚠️ Internal Error (code: -32603)" | profile 的 quota 已耗尽，AWS 拒绝请求 | 跑 `kiro-pool usage --update-state` 刷新状态；kiro-wrap 下次会自动学习并跳过该 profile |
| list 里 USAGE 列全是 `-` | 从未查询过 usage | 跑一次 `kiro-pool usage --update-state` 或 `kiro-pool list --refresh-usage` |
| login 后 list 里 TYPE 显示 `free` 但实际是 student | `--tier` 标签没打对 | `kiro-pool tag <name> student` 修正 |
| kiro-cli 登录卡住不动（VPS） | OAuth callback 打不到 VPS 的 localhost:3128 | kiro-pool 会提示贴 URL；把浏览器地址栏的 `localhost:3128/...` URL 贴回终端即可 |
| "flock timeout" | 另一个进程持有 state 锁超时 | 检查是否有 zombie wrap 进程；`kill` 掉或等 zombie_minutes 过期自动回收 |
| profile 月初仍被标为 100% | state.json 里的 resets_at 还没到 | 正常情况 pick 会自动检查 resets_at 并解禁；如果日期不对，手动 `kiro-pool usage --update-state` 刷新 |

### systemd 完整范例

> 以下以 root 用户为例。若用其他用户部署，所有 `/root` 替换为对应 home 目录，`User=` 改为实际用户。

```ini
[Unit]
Description=openab with kiro-pool
After=network.target

[Service]
Type=simple
User=root
# 启动前刷一遍 usage，避免首次撞墙（冷启动保护）
ExecStartPre=/root/.cargo/bin/kiro-pool usage --update-state
ExecStart=/root/.cargo/bin/openab
Environment=KIRO_POOL_DIR=/root/.kiro-pool
Environment=HOME=/root
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
```

`ExecStartPre` 的作用：每次 openab 启动（含 crash 重启）前，先查询所有 profile 的 credits 用量并写入 state.json。这样 kiro-wrap pick 时能立刻跳过已耗尽的 profile，而不是等第一次撞墙才被动学习。

> **修改 unit 文件后必须 reload：**
> ```bash
> sudo systemctl daemon-reload
> sudo systemctl restart openab.service
> ```
> 不执行 `daemon-reload` 直接 start/restart 会报错（systemd 仍用旧的缓存 unit）。

### 定时刷新 usage（可选）

```bash
# crontab -e
# 每小时刷一次 usage，保持 state.json 里的数据新鲜
0 * * * * /root/.cargo/bin/kiro-pool usage --update-state >/dev/null 2>&1
```

## 给 AI agent 的部署指南

> 本节是写给 **帮用户部署/配置 kiro-multi 的 coding agent**（Cursor / Claude Code / Codex / Aider 等）看的 runbook。如果你是人，可以直接跳过——README 前面的内容已经覆盖一切。如果你是 agent：动手前**完整读完本节**，它把 README 其它部分隐含的约束写明确了。

### 先识别用户目标

按用户的描述匹配以下其中**一个**意图，每个对应不同的命令序列：

| 用户意图 | 对应路径 |
|---|---|
| 本地试用，单个账号 | **路径 A — 本地单账号** |
| 本地多账号，手动 `kiro-wrap` | **路径 B — 本地多账号** |
| VPS 部署给外部集成（openab / cron / 聊天机器人）使用 | **路径 C — VPS + 集成** |
| 把账号迁移到另一台机器 | **路径 D — 在新机重新登录（**禁止** rsync）** |

如果用户意图不清晰，**只问一个澄清问题**，不要拍脑袋假设。

### 硬性约束（绝对不能违反）

1. **永远不要 `rsync` / `cp -r` / `tar` 复制 `~/.kiro-pool/profiles/` 到其他机器。** 认证 token 跟主机 keychain（macOS）或文件系统 ACL 绑定。在目标机重新 login。
2. **永远不要手改 `state.json`。** 它有 flock 保护，用 `kiro-pool` 子命令（`tag` / `release` / `clear-cooldown` / `remove`）。
3. **永远不要在 profile 目录下手动跑 `security` / `keychain` 命令。** 所有 keychain 配置走 `kiro-pool login` 和 `kiro-pool fix-keychain`。
4. **不要把 `.kiro-pool/` 提交到任何 repo。** 包含认证材料。
5. **不要给 `kiro-wrap` 加 flag** —— 所有 flag 都会透传给 `kiro-cli`。pool 配置用 env：`KIRO_POOL_DIR` / `KIRO_POOL_PROFILE` / `KIRO_WRAP_NO_STDOUT_TEE`。
6. **从 systemd / cron / openab 调 `kiro-wrap` 时，调用方 env 必须含 `HOME`。** 缺 `HOME` = 启动时硬错误。一律显式设置。
7. **冷启动后必须刷 usage**：任何全新安装或清空 `state.json` 后，第一次跑实际 session 之前一定要先跑 `kiro-pool usage --update-state`，否则首次 pick 可能命中已耗尽的 profile 并报错。
8. **一个 AWS 账号 = 一台机器上一个 profile。** 不要把同一个账号同时登录到两台机器的不同 pool —— kiro-cli 的 session state 会分叉，其中一边会静默失效。

### 路径 A — 本地单账号

```bash
# 1. 构建（只在源码安装时需要）
cargo install --path .

# 2. 确认真实 kiro-cli 至少跑过一次（让它 bootstrap bun / tui.js）
kiro-cli --version            # 失败的话，先装 kiro-cli；停止操作并告知用户

# 3. 创建 profile 并登录
kiro-pool login a --tier free   # 或 --tier student / pro / pro+ / power（按实际档位）

# 4. 验证
kiro-pool doctor
kiro-pool usage --update-state  # 冷启动必须刷一遍
kiro-pool list                  # 确认 STATUS=idle，USAGE 列有数

# 5. 使用
kiro-wrap                       # 裸跑 = chat
```

如果 `kiro-pool doctor` 报 `[FAIL]`，**停下来告诉用户**，不要自动修。**唯一例外**：报 `user keychain search list polluted` —— 这种跑 `kiro-pool fix-keychain`（仅 macOS）即可。

### 路径 B — 本地多账号

跟路径 A 一样，但 step 3 对每个账号重复。命名要有意义、和档位对应：

```bash
kiro-pool login free_1     --tier free
kiro-pool login student_1  --tier student
kiro-pool login student_2  --tier student
kiro-pool login pro_1      --tier pro
```

然后 `kiro-pool list` 应该显示所有 profile 且 TYPE 正确。pick 策略会先消耗 `free_*`，再 `student_*`，最后 `pro_*`。用户日常用 `alias kiro='kiro-wrap'`。

### 路径 C — VPS + 集成

**最容易翻车的路径。** 四种常见失败模式：

| 用户报的现象 | 根因 | 怎么验证 | 修法 |
|---|---|---|---|
| "Connection Lost" / 启动时静默退出 | 调用方 env 缺 `HOME` | 看 systemd unit / openab config | env 加 `HOME=/<用户家目录>` |
| `kiro-cli` not found | systemd `User=` 与 cargo install user 不一致 | 用 systemd user 跑 `which kiro-wrap` | 全用绝对路径 `/<家目录>/.cargo/bin/kiro-wrap`；不要依赖 PATH |
| 重启后第一个请求 `-32603` | 启动时没刷 usage | 看 `state.json` 里 `last_usage` 是不是空 | systemd unit 加 `ExecStartPre=/<家目录>/.cargo/bin/kiro-pool usage --update-state` |
| `flock timeout` / pick 失败 | `~/.kiro-pool/` owner 不对 | `ls -la ~/.kiro-pool/state.json` | `chown -R <systemd-user>:<group> ~/.kiro-pool/`；如果 openab 跑非 root 用户，就不要用 root 跑 `kiro-pool`，反之亦然 |

标准 VPS 部署 checklist（按顺序，不能跳）：

1. 确认：用哪个 Linux 用户跑集成？（如 `root` / `ubuntu` / `openab`）。下面记作 `$U`。所有路径都基于 `$U` 的家目录。
2. 用 `$U` 身份装 kiro-cli；`kiro-cli --version` 验证。
3. 用 `$U` 跑一次 `kiro-cli chat` 立即退出 —— 让它 bootstrap `bun` / `tui.js` 到 `~/.local/share/kiro-cli/`。跳这步 pool 的 symlink 无处可指。
4. 用 `$U`：`cargo install --path .`（或拷预编译 binary 到 `~/.cargo/bin/`）。
5. `kiro-pool login <name> --tier <tier>` 每个账号。VPS 上 Google/GitHub OAuth：kiro-pool 会提示你把浏览器跳转的 `localhost:3128/...` URL 贴回终端。
6. `kiro-pool doctor` —— 必须全 `[OK]`。
7. `kiro-pool usage --update-state` —— 冷启动必做。
8. 接入集成。openab + systemd 见上面 [systemd 完整范例](#systemd-完整范例) 一节。**`Environment=` 必须同时含 `KIRO_POOL_DIR` 和 `HOME`**。
9. 改完 systemd unit **一定要** `sudo systemctl daemon-reload && sudo systemctl restart <unit>`。跳过 `daemon-reload` 会用旧的缓存 unit。
10. 启动后 tail log：`journalctl -u <unit> -f`，等到看见首个成功请求。

### 路径 D — 把账号迁到新机器

**不要 rsync `~/.kiro-pool/profiles/`** —— credentials 在目标机不工作。

正确流程：

1. 新机器上从头跑路径 A 或 B。
2. 每个账号重新 `kiro-pool login`（名字保持一致也行）。
3. 新机器拿到全新的 token，绑到自己的 keychain / 文件系统。

唯一可以拷的是 `~/.kiro-pool/config.toml`（你调的 cooldown_regex / tier_model 之类）。

### 验证命令（多用）

任何改动之后跑对应的子集：

| 改了什么 | 跑什么 |
|---|---|
| install / build | `kiro-pool --version && kiro-wrap --version` |
| login | `kiro-pool doctor <name>` 然后 `kiro-pool list` |
| config 编辑 | `kiro-pool list --json`（如果 config.toml 写错会解析失败） |
| 自己改了 state | `cat ~/.kiro-pool/state.json | jq .` |

`kiro-pool doctor` 报 `[FAIL] user keychain search list polluted` 时：先跑 `kiro-pool fix-keychain --dry-run` 看会删什么，再跑 `kiro-pool fix-keychain` 应用。

### 什么时候问用户、不要自己动

- 用户没说账号是哪个 **tier**（默认 `free` 会影响 pick 优先级）。
- VPS / systemd 部署没说用哪个**用户**。
- `~/.kiro-pool/` 已经存在、你只是要加东西 —— 动之前先确认。
- `kiro-pool doctor` 失败、且不是 keychain 污染那一种。
- 用户说"想让多台机器共享账号" —— 拒绝，那是路径 D，**禁止 rsync**。

### 这个工具是什么、不是什么

- **是**：一个 per-`HOME` 的 profile 复用器，在用户**已合法持有**的 Kiro CLI 账号之间轮转。不绕过认证、不抓取 AWS、不伪造凭证。
- **不是**：账号创建器、凭证破解工具、绕配额的手段。用户问这种事 —— 拒绝。

## License

MIT —— 见 [LICENSE](LICENSE)。


