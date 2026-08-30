# Hive

> 面向 CLI agent 的 tmux 协作 runtime。`claude`、`codex`、`grok` 成员各自跑在自己的引擎里，通过各引擎的原生投递通道交换 `<HIVE>` 消息，共用一份注册表作为真相层。

[English](README.md) · **简体中文** · [日本語](README.ja.md)

_本文档以 [README.md](README.md) 为准，翻译可能滞后于英文原版。_

## 什么是 Hive

Hive 是面向 agent 的 runtime，不是靠人手动驱动的 CLI。一个 team 是注册表里的名册（`$HIVE_HOME/state/teams/` 下每队一个 JSON）加上每个成员各自的引擎；tmux 窗口是 `hive attach` 在其上渲染的显示层。

这条边界由实现保证。tmux 外的 `hive create` 注册的是 headless 团；没有显示层时 `hive spawn` 出来的成员只有引擎没有 pane，照样收消息、回报、被 kill；窗口可以之后用 `hive attach <team>` 实体化，一个成员一个 pane。tmux 不持有任何真相，关闭窗口不丢失状态。

派活、发消息、读 runtime 状态都在 agent 会话里完成，由 agent 执行命令。给人的入口是插件 skill `/hive:hive [team]`：不带参数按处境创建或加入，带队名则加入该队，队不存在则创建。仍有一小部分命令由人执行：安装插件、看会话 transcript（`hive view`）、弹窗编辑器（`hive cvim` / `hive vim`）、分屏 fork，以及本地开发安装。

## 安装

Hive 是单个 Rust 二进制，从 checkout 编译：

```bash
git clone https://github.com/notdp/hive.git
cd hive
cargo install --path crates/hive
```

仓库同时是两个 CLI 的插件 marketplace，插件分发的是教 agent 协议的 skill：

```bash
# Claude Code
claude plugin marketplace add notdp/hive
claude plugin install hive@hive

# Codex
codex plugin marketplace add https://github.com/notdp/hive.git
codex plugin add hive@hive
```

CLI 请先自行安装。插件的 `SessionStart` hook 看起来会代为安装，实际不会：它的收敛步骤仍然对这个仓库调用 `pipx install`，而该路径早于 Rust 改写，仓库从那以后就没有 `pyproject.toml` 了，因此这条路装不出二进制，hook 的后半段（打开 Claude 侧的 marketplace 自动更新）也不会执行。唯一能收敛的路径是 PATH 上已有足够新的 `hive`：此时该检查不安装任何东西，直接放行。

依赖：

- `tmux` 3.2+ —— `hive cvim` / `hive vim` 的弹窗要 3.2+；`hive view` 选主题时发的裸 OSC 11 背景色查询，也要 3.2 起才会在 pane 里被应答
- Rust 工具链（编译用）
- `python3` —— `hive flow run` 会 exec 解释器运行脚本，配套的 `hive.flow` 客户端是内嵌的；notify 弹窗也是一段 python heredoc
- 至少一种 agent CLI：`claude`、`codex` 或 `grok`

## 在 agent 会话中开始

```bash
# 一次性设置：在 shell rc 里加 eval "$(hive shell-init zsh)"
# 在 tmux 里通过 hive 的启动器启动你要用的 agent
$ hclaude      # 或：hcodex / hgrok

# 在 agent 会话里输入：
/hive:hive
```

agent 会把当前 pane 立为这个队的 orch，之后按任务需要 spawn 成员。此后的交互发生在与 agent 的对话中，由 agent 管理这支队伍。

## 把 `hive fork` 绑到按键

终端的快捷键绑定没法直接跑 shell 命令，所以绑定发的是一个裸转义字节，由 tmux
接住。macOS + Ghostty 下，Cmd+Shift+F 发出 ESC f，tmux 执行 fork：

```
# ~/.config/ghostty/config
keybind = cmd+shift+f=text:\x1bf

# ~/.tmux.conf
bind -n M-f run-shell -b 'hive fork --pane "#{pane_id}"'
```

不加 `-b`，tmux server 会在 fork 期间阻塞。`--pane` 同样必需：绑定从 pane 外
触发，自动探测会认错源 pane。

## 为什么 transcript 镜像是只读的

交互式的 Claude 会话没有可挂的 pty（`claude attach` 只认 job），但它的 transcript 会随 turn 推进逐条追加，因此按这个文件渲染出来的就是一份实时镜像，且结构上无法回打。`hive view` 就是这个渲染器。

一个 claude 成员的 sessionId 如果查不到 bg job 记录，它就是交互式会话（桌面 `ccd`、被 join 收编的会话），`hive attach` 会自动给它挂上这个镜像：此时 resume 会 fork 出第二个引擎，抢走这个成员的投递，所以这种成员的 pane 挂的是镜像而不是 resume，pane 因此是只读的。投递不受影响：查不到 job 记录这同一个判断会让 `hive send` 直接投给活着的交互式会话而不是 pane。能向该成员输入的只有持有该会话的 app。

## 升级

```bash
git pull && cargo install --path crates/hive
```

插件 manifest 的版本号与 CLI 版本锁在一起，因此一次发版同时带上插件更新。Claude Code 这边，bootstrap hook 写入 `extraKnownMarketplaces` 那条记录之后 marketplace 就自动更新；如果只设了 `DISABLE_AUTOUPDATER` 而没设 `FORCE_AUTOUPDATE_PLUGINS`，这一步会被跳过，此时需要手动执行 `claude plugin update hive@hive`。Codex 在 add 时对 marketplace 取快照，之后不会自行刷新；刷新需执行 `codex plugin marketplace upgrade hive`。

## 开发

安装到全局的 `hive` 是 live agent 的传输通道。开发 Hive 本身时让它停在已提交的 checkout 上，不要用队伍正在使用的脏 worktree 执行 `cargo install`。需要插件物化或 hived 行为的手工验证，使用一次性的 `HIVE_HOME`、`CLAUDE_HOME`、`CODEX_HOME` 和临时 team，不要动 live 队的 hived。测试通道与仓库约定见 [AGENTS.md](AGENTS.md)。

## 文档

- [`docs/runtime-model.md`](docs/runtime-model.md) —— 注册表与显示层的身份之分、各 CLI 的原生 runtime 来源，以及 `busy` / `inputState` / `turnPhase`
- [`docs/daemon-control-socket.md`](docs/daemon-control-socket.md) —— Claude supervisor daemon 的控制协议，其 `op:"reply"` 是 hive 的投递主道
- [`plugins/hive/skills/hive/SKILL.md`](plugins/hive/skills/hive/SKILL.md) —— `/hive:hive` 载入 agent 的协作协议

## License

[GPL-3.0-or-later](LICENSE) © 2026 notdp
