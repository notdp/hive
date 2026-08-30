# Hive

> 面向 CLI agent 的 tmux 协作 runtime。`claude`、`codex`、`grok` 成员各自跑在自己的引擎里，通过各引擎的原生投递通道交换 `<HIVE>` 消息，共用一份注册表作为真相层。

[English](README.md) · **简体中文** · [日本語](README.ja.md)

_本文档以 [README.md](README.md) 为准，翻译可能滞后于英文原版。_

## 什么是 Hive

Hive 是给 agent 用的 runtime，不是一个主要靠人手动驱动的 CLI。一个 team = 注册表里的名册（`$HIVE_HOME/state/teams/` 下每队一个 JSON）+ 各自跑在引擎里的成员；tmux 窗口只是画在这之上的显示层，由 `hive attach` 渲染，仅此而已。

这条边界是落在实现里的。tmux 外的 `hive create` 注册的是 headless 团，没有显示层时 `hive spawn` 出来的成员只有引擎没有 pane，照样收消息、回报、被 kill；想看的时候再 `hive attach <team>`，一个成员一个 pane 把窗口长出来。tmux 手里没有任何算真相的东西，所以关掉窗口不损失什么。

派活、发消息、看 runtime 状态这些日常动作都在 agent 会话里完成，由你的 agent 去跑命令。给人的入口是插件 skill `/hive:hive [team]`：不带参数就按处境创建或加入，带队名就加入该队，队不存在则建出来。仍有一小部分命令由你来运行：安装插件、看会话 transcript（`hive view`）、弹窗编辑器（`hive cvim` / `hive vim`）、分屏 fork，以及本地开发安装。

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

CLI 请自己先装好。插件的 `SessionStart` hook 看着像会替你装，其实装不出来：它的收敛这一步仍然去调 `pipx install` 打这个仓库，那是 Rust 改写之前的老路，而仓库从那以后就没有 `pyproject.toml` 了——这条路装不出二进制，hook 的后半段（打开 Claude 侧的 marketplace 自动更新）也就永远跑不到。只有 PATH 上已经有足够新的 `hive` 时，这个检查才会什么都不装直接放行，那是唯一能收敛的路径。

依赖：

- `tmux` 3.2+ —— `hive cvim` / `hive vim` 的弹窗要 3.2+；`hive view` 选主题时发的裸 OSC 11 背景色查询，也要 3.2 起才会在 pane 里被应答
- Rust 工具链（编译用）
- `python3` —— `hive flow run` 会 exec 解释器去跑脚本，配套的 `hive.flow` 客户端是内嵌的；notify 弹窗也是一段 python heredoc
- 至少一种 agent CLI：`claude`、`codex` 或 `grok`

## 在 agent 会话中开始

```bash
# 一次性设置：在 shell rc 里加 eval "$(hive shell-init zsh)"
# 在 tmux 里通过 hive 的启动器启动你要用的 agent
$ hclaude      # 或：hcodex / hgrok

# 在 agent 会话里输入：
/hive:hive
```

agent 会把当前 pane 立为这个队的 orch，之后按任务需要把成员 spawn 出来。从这里开始，你和 agent 对话；agent 去管这支队。

## 把 `hive fork` 绑到按键

终端的快捷键绑定没法直接跑 shell 命令，所以绑定发的是一个裸转义字节，由 tmux
接住。macOS + Ghostty 下，Cmd+Shift+F 发出 ESC f，tmux 执行 fork：

```
# ~/.config/ghostty/config
keybind = cmd+shift+f=text:\x1bf

# ~/.tmux.conf
bind -n M-f run-shell -b 'hive fork --pane "#{pane_id}"'
```

`-b` 是承重的：不加它 tmux server 会在 fork 期间阻塞。`--pane` 同理——绑定是从
pane 外面触发的，自动探测会认错源 pane。

## 为什么 transcript 镜像是只读的

交互式的 Claude 会话没有可挂的 pty——`claude attach` 只认 job——但它的 transcript 是随 turn 推进一条条追加的，所以照着这个文件渲染出来的就是一面忠实的实时镜子，而且从构造上就打不回去。`hive view` 就是这个东西。

一个 claude 成员的 sessionId 如果查不到 bg job 记录，那它就是交互式会话（桌面 `ccd`、被 join 收编的会话），`hive attach` 会自己给它挂上这面镜子：此时去 resume 会 fork 出第二个引擎把这个成员的投递抢走，所以这种成员的 pane 挂的是镜像而不是 resume。代价很实在：这个 pane 是只读的。投递不受影响——查不到 job 记录这同一个判断，会让 `hive send` 直接投给那个活着的交互式会话而不是 pane——但除了持有该会话的那个 app，谁也没法对这个成员打字。

## 升级

```bash
git pull && cargo install --path crates/hive
```

插件 manifest 的版本号与 CLI 版本锁在一起，所以发一次版就等于带上了插件更新。Claude Code 这边，bootstrap hook 写入 `extraKnownMarketplaces` 那条记录之后 marketplace 就自动更新；但如果只设了 `DISABLE_AUTOUPDATER` 而没设 `FORCE_AUTOUPDATE_PLUGINS`，这一步会被跳过，那就得手动 `claude plugin update hive@hive`。Codex 是在 add 的那一刻把 marketplace 快照下来，之后不会自己刷新——要跑 `codex plugin marketplace upgrade hive`。

## 开发

装到全局的 `hive` 是活着的 agent 传输通道。开发 Hive 本身的时候让它停在已提交的 checkout 上，绝不要拿正在被队伍使用的脏 worktree 去 `cargo install`。需要插件物化或者 hived 行为的手工验证，用一次性的 `HIVE_HOME`、`CLAUDE_HOME`、`CODEX_HOME` 和临时 team，不要动 live 队的 hived。测试通道与仓库约定见 [AGENTS.md](AGENTS.md)。

## 文档

- [`docs/runtime-model.md`](docs/runtime-model.md) —— 注册表与显示层的身份之分、各 CLI 的原生 runtime 来源，以及 `busy` / `inputState` / `turnPhase`
- [`docs/daemon-control-socket.md`](docs/daemon-control-socket.md) —— Claude supervisor daemon 的控制协议，其 `op:"reply"` 就是 hive 的投递主道
- [`plugins/hive/skills/hive/SKILL.md`](plugins/hive/skills/hive/SKILL.md) —— `/hive:hive` 载入 agent 的那份协作协议

## License

[GPL-3.0-or-later](LICENSE) © 2026 notdp
