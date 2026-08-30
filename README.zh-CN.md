# Hive

> 面向 CLI agent 的 tmux 协作 runtime。`claude`、`codex`、`grok` 成员各自跑在自己的引擎里，通过各引擎的原生投递通道交换 `<HIVE>` 消息，共用一份注册表作为真相层。

[English](README.md) · **简体中文** · [日本語](README.ja.md)

_本文档以 [README.md](README.md) 为准，翻译可能滞后于英文原版。_

## 什么是 Hive

Hive 是给 agent 用的 runtime，不是一个主要靠人手动驱动的 CLI。一个 team = 注册表里的名册（`$HIVE_HOME/state/teams/` 下每队一个 JSON）+ 各自跑在引擎里的成员；tmux 窗口只是可选的显示层，由 `hive attach` 画出来。派活、发消息、看 runtime 状态这些日常动作都在 agent 会话里完成，由你的 agent 去跑命令。

给人的入口是插件 skill `/hive:hive [team]`：不带参数就按处境创建或加入，带队名就加入该队，队不存在则建出来。

仍有一小部分命令由你来运行：安装插件、看会话 transcript（`hive view`）、弹窗编辑器（`hive cvim` / `hive vim`）、分屏 fork，以及本地开发安装。

## 安装

Hive 是单个 Rust 二进制，从 checkout 编译：

```bash
git clone https://github.com/notdp/hive.git
cd hive
cargo install --path crates/hive
```

仓库同时是两个 CLI 的插件 marketplace，插件里装的是教 agent 协议的那份 skill：

```bash
# Claude Code
claude plugin marketplace add notdp/hive
claude plugin install hive@hive

# Codex
codex plugin marketplace add https://github.com/notdp/hive.git
codex plugin add hive@hive
```

CLI 请自己先装好，别等插件的 `SessionStart` hook。那个 hook（`plugins/hive/scripts/bootstrap.py`）干两件事：先收敛 CLI，再打开 Claude 侧的 marketplace 自动更新。但它的收敛这一步仍然去调 `pipx install git+https://github.com/notdp/hive`（`bootstrap.py:114`），那是 Rust 改写之前的老路——仓库已经没有 `pyproject.toml`，这条路装不出二进制，后半段也就永远跑不到。只有 PATH 上已经有 ≥ 0.10.1 的 `hive` 时，它才会返回 `already meets minimum` 什么都不装，然后继续往下走。

依赖：

- `tmux` —— `hive cvim` / `hive vim` 的弹窗需要 3.2+；`hive view` 选主题时发的裸 OSC 11 背景色查询也要 3.2+ 才会在 pane 里被应答（`crates/hive/src/view_theme.rs:349`）
- Rust 工具链（编译用）
- `python3` —— `hive flow run` 会 exec 解释器去跑脚本，配套的 `hive.flow` 客户端是内嵌的（`crates/hive/src/cli/rest.rs:1094`）；notify 弹窗也是一段 python heredoc（`crates/hive/src/notify_ui.rs:242`）
- 至少一种 agent CLI：`claude`、`codex` 或 `grok`

## 在 agent 会话中开始

```bash
# 一次性设置：在 shell rc 里加 eval "$(hive shell-init zsh)"
# 在 tmux 里通过 hive 的启动器启动你要用的 agent
$ hclaude      # 或：hcodex / hgrok

# 在 agent 会话里输入：
/hive:hive
```

skill 加载后，agent 会跑 `hive create` 把当前 pane 立为这个队的 orch，之后按任务需要用 `hive spawn` 造成员。从这里开始，你和 agent 对话；agent 去管这支队。

这一整套都不依赖 tmux。tmux 外的 `hive create` 注册的是 headless 团，没有显示层时 `hive spawn` 出来的成员只有引擎没有 pane，照样收消息、回报、被 kill；想看的时候再 `hive attach <team>`，一个成员一个 pane 把窗口长出来。

## 手动命令

人通常会手动运行的命令：

```bash
# 插件
hive plugin enable notify --plain # hived 空闲监视开关（`hive notify` 手动调用不受影响）
hive plugin list --plain          # 人读格式（默认输出是 JSON）

# 只读 transcript 镜像
hive view <session-id>            # 实时跟随一个 Claude 会话；按键不会传回去

# 弹窗编辑器（tmux 3.2+）
hive cvim                         # 把上一条 assistant 消息拉进 vim 改完发回去
hive vim                          # 在空 buffer 里写，写完发给 agent pane

# 将当前 agent 会话 fork 到新的分屏 pane
hive fork                         # 自动判断分屏方向
hive vfork                        # 垂直分屏
hive hfork                        # 水平分屏
```

`hive view` 渲染的是 `~/.claude/projects/*/<session-id>.jsonl`（`crates/hive/src/transcript_view.rs:44`）。交互式的 Claude 会话没有可挂的 pty——`claude attach` 只认 job——但它的 transcript 是随 turn 推进一条条追加的，所以照着这个文件渲染出来的就是一面忠实的实时镜子，而且从构造上就打不回去。在 tty 上它是一个 ratatui pager：`↑↓` 选块，`←→` 折叠展开，`Enter` 全屏看，`Ctrl+o` 切换密度，`/` 唤出命令面板（`/theme`、`/view`、`/find`、`/quit`），`q` 退出。被管道接走或重定向时退化成纯 ANSI 流（`transcript_view.rs:1622`）。主题优先看 `HIVE_VIEW_THEME=light|dark|auto`，其次 `view.theme` 设置，都没有就自动探测，探不出来落到 light（`view_theme.rs:281`）。

`hive attach` 会自己挑它。一个 claude 成员的 sessionId 如果查不到 bg job 记录，那它就是交互式会话（桌面 `ccd`、被 join 收编的会话），此时去 resume 会 fork 出第二个引擎把这个成员的投递抢走——所以这种成员的 pane 挂的是 `hive view` 而不是 resume（`crates/hive/src/cli/rest.rs:1268`）。代价很实在：这个 pane 是只读的。投递不受影响——查不到 job 记录这同一个判断，会让 `hive send` 直接投给那个活着的交互式会话而不是 pane（`crates/hive/src/agent.rs:759`）——但除了持有该会话的那个 app，谁也没法对这个成员打字。

在 Claude Code / Codex 里，请通过 shell escape 调用这些命令，例如：`!hive cvim`、`!hive vfork`、`!hive fork` 等。

把 `hive fork` 绑到键盘快捷键上，配合 tmux 用起来很顺手。示例（macOS 上的 Ghostty + tmux）——Cmd+Shift+F 将当前 pane fork；请按你的终端自行调整按键：

```
# ~/.config/ghostty/config
keybind = cmd+shift+f=text:\x1bf

# ~/.tmux.conf
bind -n M-f run-shell -b 'hive fork --pane "#{pane_id}"'
```

其它命令，例如 `hive send`、`hive team`、`hive spawn`、`hive doctor <agent>` 等，都是按“由 agent 调用”来设计的。你手动运行也可以，但那属于调试 / 高阶路径，不是默认 happy path。

## 升级

CLI 靠重新编译一份已提交的 checkout：

```bash
git pull && cargo install --path crates/hive
```

插件 manifest 的版本号与 CLI 版本锁在一起，所以发一次版就等于带上了插件更新。Claude Code 这边，bootstrap hook 写入 `extraKnownMarketplaces.hive` 且 `autoUpdate: true` 之后 marketplace 就自动更新；但如果只设了 `DISABLE_AUTOUPDATER` 而没设 `FORCE_AUTOUPDATE_PLUGINS`，这一步会被跳过，那就得手动 `claude plugin update hive@hive`。Codex 是在 add 的那一刻把 marketplace 快照下来，之后不会自己刷新——要跑 `codex plugin marketplace upgrade hive`。

## 给贡献者

```bash
cargo nextest run                 # 全量 Rust 测试
python -m pytest tests/e2e -q     # 针对 target/debug/hive 的 tmux 黑盒流程
```

nextest 是硬要求不是偏好：测试会随意改环境变量，而 `cargo test` 把它们跑在同一个进程里，互相污染。

每次装完 live 版都要跑一遍装后验收——它为每种 CLI 起一个真成员，断言单测层看不见的那些 oracle（回信身份、`capture-pane -e` 读出的 pane 颜色、nonce 因果、headless claude 语义验尸官）：

```bash
HIVE_ACCEPTANCE=1 HIVE_ACCEPTANCE_CLIS=claude,codex,grok python -m pytest tests/acceptance -q
```

装到全局的 `hive` 是活着的 agent 传输通道。开发 Hive 本身的时候让它停在已提交的 checkout 上，绝不要拿正在被队伍使用的脏 worktree 去 `cargo install`。需要插件物化或者 hived 行为的手工验证，用一次性的 `HIVE_HOME`、`CLAUDE_HOME`、`CODEX_HOME` 和临时 team/window，不要动 live 队的 hived。仓库约定见 [AGENTS.md](AGENTS.md)。

## 文档

- [`docs/runtime-model.md`](docs/runtime-model.md) —— 注册表与显示层的身份之分、各 CLI 的原生 runtime 来源，以及 `busy` / `inputState` / `turnPhase`
- [`docs/daemon-control-socket.md`](docs/daemon-control-socket.md) —— Claude supervisor daemon 的控制协议，其 `op:"reply"` 就是 hive 的投递主道
- [`plugins/hive/skills/hive/SKILL.md`](plugins/hive/skills/hive/SKILL.md) —— `/hive:hive` 载入 agent 的那份协作协议

## License

[GPL-3.0-or-later](LICENSE) © 2026 notdp
