---
name: crew
description: CREW entry skill. 用户打 /crew 表示要把当前 pane 升级成 CREW orchestrator 并启动 crew 闭环。skill 内容 = 跑 `hive crew init`,把当前 pane 搬到新 window,布好 challenger,并自动 dispatch /crew-orch 接管 duty。
disable-model-invocation: true
---

# CREW — entry

你被 `/crew` 触发,用户要启动 CREW 闭环。**你做一件事**:在当前 pane 执行:

```bash
hive crew init
```

完事。执行后你会看到:

- 当前 pane 切到新的 crew window,orch 身份带过去
- 同 window 出现 challenger(异族 CLI,claude↔codex;droid 默认 claude)
- `/crew-orch` 自动接管 orch pane,本 skill 退场

用户想显式指定 crew 实例名可传 `--name <name>`;不传就由 CLI 自动分配。

## 前置

- **当前 pane 正在跑 agent CLI**(claude / codex / droid),不是光秃秃 shell
- workspace 不需要先 `hive init` —— `hive crew init` 可独立运行,未 init 时自动建 team / workspace

## 边界

只跑 `hive crew init`,别手动做它已包办的:窗口布局、challenger spawn、workspace / agent name 推断。planning / 拆 feature 是 `/crew-orch` 接管后的 duty。

## 报错兜底

- `hive: command not found` → 告诉用户 `pipx install git+https://github.com/notdp/hive.git`
- 报 "not an agent pane" → 当前 pane 不是 agent CLI,换到跑着 claude/codex/droid 的 pane 再 `/crew`

## 模型异质

`hive crew init` 默认会用 anti-orch 家族 CLI 起 challenger(claude↔codex;droid 默认 claude)。若当前 CLI 是 droid 但跑的是 Anthropic 模型(opus / sonnet),显式 override:

```bash
hive crew init --peer-cli codex
```
