# Hive

> 面向 CLI agent 的 tmux 协作 runtime。`claude`、`codex`、`grok` 成员各自跑在自己的引擎里，通过各引擎的原生投递通道交换 `<HIVE>` 消息，共用一份注册表作为真相层。

[English](README.md) · **简体中文** · [日本語](README.ja.md)

_本文档以 [README.md](README.md) 为准，翻译可能滞后于英文原版。_

## 什么是 Hive

Hive 是面向 agent 的 runtime，不是靠人手动驱动的 CLI。一个 team 是注册表里的名册（每队一个目录 `$HIVE_HOME/teams/<team>/`，放 `team.json`，缺省也是它的 workspace）加上每个成员各自的引擎；tmux 窗口是画在名册之上的显示层——建团即有。

这条边界由实现保证。tmux 外的 `hive create` 建一个以团名命名的 detached tmux session 放团窗口；`hive spawn` 从任何地方把 pane 切进团窗口；`hive attach` 跳过去，窗口被关或 tmux 重启时先按名册重建——引擎从来不在窗口里。tmux 不持有任何真相，关闭窗口不丢失状态。

派活、发消息、读 runtime 状态都在 agent 会话里完成，由 agent 执行命令。给人的入口是插件 skill `/hive:hive [team]`：不带参数按处境创建或加入，带队名则加入该队，队不存在则创建。仍有一小部分命令由人执行：安装插件、看会话 transcript（`hive view`）、弹窗编辑器（`hive cvim` / `hive vim`）、分屏 fork，以及本地开发安装。

## 安装

Hive 是单个 Rust 二进制。[GitHub Releases](https://github.com/notdp/hive/releases) 提供 macOS 和 Linux（aarch64 与 x86_64）的预编译二进制：

```bash
curl -fsSL https://github.com/notdp/hive/releases/latest/download/hive-installer.sh | sh
```

有 Rust 工具链的话还有两条路：[`cargo binstall`](https://github.com/cargo-bins/cargo-binstall) 拉取同一份预编译 release（不编译），`cargo install` 从源码编译：

```bash
cargo binstall --git https://github.com/notdp/hive hive
# 或
cargo install --git https://github.com/notdp/hive hive
```

插件——教 agent 协议的那份 skill——内嵌在二进制里，由 `hive` 在 `$HIVE_HOME` 下物化出一个本地 marketplace 来提供。一条命令就为 PATH 上的每个 agent CLI 注册并安装它（重跑可修复安装）：

```bash
hive plugin setup
```

它在底下物化 marketplace，再对 claude（2.1.229+）和 codex 各执行 `plugin marketplace add` + install。claude 侧的 marketplace 条目是 command source——Claude 每个 session 重跑一次 `hive plugin sync`，所以 skill 更新随二进制走；codex 侧插件不带任何 hook（hook 会卡在 codex 的 hook 审阅对话框后面）——hive 自己的 codex 启动路径在二进制版本变化时、引擎启动前重新 add 插件。不从远端拉取任何东西，也不改任何 settings。

依赖：

- `tmux` 3.5+ —— hive 在每个团 session 上挂着一个 control-mode 客户端（hived 的 pane 监视器），而 tmux 会用这个从未拿到真实颜色的客户端去回答 pane 的 OSC 10/11 颜色查询：tmux 3.4 上团 pane 里的 codex 和 `hive view` 被告知背景是黑的，在浅色终端里画成深色。3.5 起 hive 自己替 pane 上报颜色（`refresh-client -r`，依次看 `view.theme`、`HIVE_APPEARANCE` / `COLORFGBG`，缺省浅色）。`hive create`、`hive doctor`、`hive plugin setup` 在旧 tmux 上会警告。`hive cvim` / `hive vim` 的弹窗要 3.2+
- Rust 工具链 —— 仅源码编译这条路需要；installer 装的是预编译二进制
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

一个 claude 成员的 sessionId 如果查不到 bg job 记录，它就是交互式会话（桌面 `ccd`、被 join 收编的会话），显示层会自动给它挂上这个镜像：此时 resume 会 fork 出第二个引擎，抢走这个成员的投递，所以这种成员的 pane 挂的是镜像而不是 resume，pane 因此是只读的。投递不受影响：查不到 job 记录这同一个判断会让 `hive send` 直接投给活着的交互式会话而不是 pane。能向该成员输入的只有持有该会话的 app。

镜像就是一个普通 pane，排在团窗口第一格，默认就在：没有任何规则会主动不画它，除非你自己收起。收起靠 `hive mirror off`（`on` 展开，不带参数则切换）、状态栏上的 orch 芯片，或 `prefix+m`。三者做的是同一件事：用 `break-pane` / `join-pane` 把这个 pane 在团窗口和团 session 的一个隐藏窗口之间搬来搬去，viewer 进程从不重启；选择记在窗口的 `@hive-mirror` 上，`hive attach` 修复显示时照它办。

hive 自己建的团 session——tmux 外 `hive create`、`hive attach` 重建丢失的窗口——带一根自己的两行状态栏，只动 session 级选项，你全局的 tmux 状态栏原样不动。第一行：团名芯片；orch 芯片，镜像 pane 在窗口里时是 ` ▾ orch `，收起时是 ` ▴ orch `（点一下切换）；每个 pane 一个芯片——成员名前面是 ● 忙、○ 闲，或 ✱ 未读（投递到了、成员还没开始处理）/ notify 后待关注，当前 pane 加粗，点一下选中那个 pane；然后是 `hive pr set` 盖过章的 `PR<n>`、session 名和时钟。第二行是 ticker：最新两条 bus 消息，格式 `from → to · 时间 · "开头几个字"`，有待处理的 notify 时其文本排在前面。状态栏上的每一项都是 CLI 或 hived 写下的 tmux 选项，它从不跑 shell 命令；点状态栏的其他地方仍是 tmux 原本的行为。

## 升级

重跑[安装](#安装)里的 installer 一行命令，它总是拉取最新 release。发版方式是推一个与 crate 版本一致的 `v*` tag；CI（cargo-dist）编译各平台二进制并发布 GitHub Release。

skill 更新随二进制走：claude 侧 marketplace 的 command source 每个 session 重跑 `hive plugin sync`，自动拿到变更内容；codex 侧当缓存里没有当前二进制版本的条目时，hive 的启动路径会重新 add 插件。插件 manifest 的版本号与 CLI 版本锁在一起——codex 缓存正是以这把锁为键。

## 开发

安装到全局的 `hive` 是 live agent 的传输通道。开发 Hive 本身时让它停在已提交的 checkout 上，不要用队伍正在使用的脏 worktree 执行 `cargo install`。需要插件物化或 hived 行为的手工验证，使用一次性的 `HIVE_HOME`、`CLAUDE_HOME`、`CODEX_HOME` 和临时 team，不要动 live 队的 hived。测试通道与仓库约定见 [AGENTS.md](AGENTS.md)。

## 文档

- [`docs/runtime-model.md`](docs/runtime-model.md) —— 注册表与显示层的身份之分、各 CLI 的原生 runtime 来源，以及 `busy` / `inputState` / `turnPhase`
- [`docs/transcript-view.md`](docs/transcript-view.md) —— `hive view` 画的是什么：JSONL → `DisplayBlock` 的解析模型、viewer 的 chrome、主题解析
- [`docs/daemon-control-socket.md`](docs/daemon-control-socket.md) —— Claude supervisor daemon 的控制协议，其 `op:"reply"` 是 hive 的投递主道
- [`plugins/hive/skills/hive/SKILL.md`](plugins/hive/skills/hive/SKILL.md) —— `/hive:hive` 载入 agent 的协作协议

## License

[GPL-3.0-or-later](LICENSE) © 2026 notdp
