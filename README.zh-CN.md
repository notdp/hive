# Hive

> 面向 CLI agent 的 tmux 协作 runtime。`claude` 与 `codex` 通过内联 `<HIVE>` 消息、可追踪的投递状态和 handoff thread 彼此协作。

[English](README.md) · **简体中文** · [日本語](README.ja.md)

_本文档以 [README.md](README.md) 为准，翻译可能滞后于英文原版。_

## 什么是 Hive

Hive 是给 agent 用的 runtime，不是一个主要靠人手动驱动的 CLI。日常工作，例如发消息、在线程里回复、交接任务、追踪投递，都是在 agent 会话里完成，由你的 agent 去跑命令。对人来说，最主要的日常入口是 `/hive`：它会把 Hive skill 加载进 agent，让它完成团队初始化。

仍有一小部分命令由你来运行：安装插件、检查 skill 漂移、使用弹窗编辑器（`hive cvim` / `hive vim`），以及本地开发安装。

## 安装

```bash
# Hive CLI
pipx install git+https://github.com/notdp/hive.git

# Hive skill，适用于 Claude Code / Codex
npx skills add https://github.com/notdp/hive -g --all
```

依赖：

- `tmux`（`hive cvim` / `hive vim` 这类弹窗辅助命令需要 3.2+）
- Python 3.11+
- 至少一种 agent CLI：`claude` 或 `codex`

## 在 agent 会话中开始

```bash
# 一次性设置：在 shell rc 里加 eval "$(hive shell-init zsh)"
# 在 tmux 里通过 hive 的启动器启动你要用的 agent
$ hclaude      # 或：hcodex

# 在 agent 会话里输入：
/hive
```

skill 加载后，agent 会运行 `hive init`，把当前 tmux window 绑定成一个 team，并自动与一个空闲的异构 peer 配对：如果能找到现成的 pane 就直接附着，否则再新开一个 pane。从这里开始，你和 agent 对话；agent 再和它的 peer 协作。

## 手动命令

人通常会手动运行的命令：

```bash
# 插件
hive plugin enable notify         # 给人的通知弹窗
hive plugin list

# 诊断
hive doctor --skills              # 升级后检查 hive skill 是否漂移

# 弹窗编辑器（tmux 3.2+）
hive cvim                         # tmux 弹窗编辑器
hive vim                          # 单 pane 变体

# 将当前 agent 会话 fork 到新的分屏 pane
hive fork                         # 自动判断分屏方向
hive vfork                        # 垂直分屏
hive hfork                        # 水平分屏
```

在 Claude Code / Codex 里，请通过 shell escape 调用这些命令，例如：`!hive cvim`、`!hive vfork`、`!hive fork` 等。

把 `hive fork` 绑到键盘快捷键上，配合 tmux 用起来很顺手。示例（macOS 上的 Ghostty + tmux）——Cmd+Shift+F 将当前 pane fork；请按你的终端自行调整按键：

```
# ~/.config/ghostty/config
keybind = cmd+shift+f=text:\x1bf

# ~/.tmux.conf
bind -n M-f run-shell -b 'hive fork --pane "#{pane_id}"'
```

其它命令，例如 `hive send`、`hive reply`、`hive team`、`hive doctor <agent>`、`hive handoff` 等，都是按“由 agent 调用”来设计的。你手动运行也可以，但那属于调试 / 高阶路径，不是默认 happy path。

## 升级

```bash
pipx upgrade hive           # 升级 CLI
npx skills update hive -g   # 升级 skill（仅适用于从 GitHub 安装的版本）
```

CLI 和 skill 需要分别升级。升级 CLI 不会自动刷新 skill。当 skill 过期时，在 agent pane 里运行 `hive` 命令会在 stderr 给出提示，而 `hive doctor --skills` 会显示具体的不匹配信息。

如果你使用的是本地 checkout，`skills update` 不能刷新这类安装方式，见下方“给贡献者”一节。

## 给贡献者

如果你是在当前 checkout 上开发，请不要从 GitHub 安装，而是直接从本地仓库安装：

```bash
python3 -m pip install -e .
npx skills add "$PWD" -g --all     # 本地 checkout 不会被 `skills update` 跟踪；要刷新时重新运行这条
PYTHONPATH=src python -m pytest tests/ -q
```

完整的修改后刷新流程（install + skill refresh + plugin re-enable）以及仓库约定，见 [AGENTS.md](AGENTS.md)。

## 文档

- [`docs/runtime-model.md`](docs/runtime-model.md) — runtime 字段语义（`busy`、`inputState`、`turnPhase`）
- [`docs/transcript-signals.md`](docs/transcript-signals.md) — Claude 的 transcript 解析规则
- [`skills/hive/SKILL.md`](skills/hive/SKILL.md) — 运行时由 Hive skill 加载的 agent 行为 / prompt contract

## License

[GPL-3.0-or-later](LICENSE) © 2026 notdp
