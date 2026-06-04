---
name: cell
description: CELL entry skill. 用户打 /cell 表示要把当前 pane 起成一个 cell 的 worker，并配一个异族 validator。skill 内容 = 跑 hive cell init，布好 validator，自动 dispatch /cell-worker 接管。
disable-model-invocation: true
---

# CELL — entry

你被 `/cell` 触发，用户要起一个 cell。**你做一件事**：在当前 pane 执行：

```bash
hive cell init
```

完事。执行后：

- 当前 pane 成为 worker；按 window pane 数，旁边 spawn 或收编一个异族 validator（claude↔codex；droid 默认 claude）
- `/cell-worker` 自动接管本 pane，本 skill 退场

## 前置

- **当前 pane 正在跑 agent CLI**（claude / codex / droid），不是光秃秃 shell
- 不需要先 `hive init` —— `hive cell init` 可独立运行

## 边界

只跑 `hive cell init`，别手动做它已包办的：窗口布局、validator spawn / 收编、workspace 推断。

## 报错兜底

- `hive: command not found` → 告诉用户 `pipx install git+https://github.com/notdp/hive.git`
- 报 "becomes the worker" 失败 → 当前 pane 不是 agent CLI，换到跑着 claude/codex/droid 的 pane 再 `/cell`

## 模型异质

`hive cell init` 默认用 anti-worker 家族起 validator。若当前 CLI 是 droid 但跑 Anthropic 模型（opus / sonnet），显式 override：

```bash
hive cell init --validator-cli codex
```
