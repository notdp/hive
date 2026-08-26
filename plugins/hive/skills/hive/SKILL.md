---
name: hive
description: Hive 是 tmux 里的多 agent 协作 runtime。当收到 HIVE 消息、被拉进 Hive team、被派发任务、或要发起协作拆任务派成员时使用；用于确认身份、取成员/编排协议、收发消息。
---

# Hive — agent 协作入口

Hive 让多个 agent 在 tmux 里用 `<HIVE ...>` 消息协作。这个文件只是发现入口；真正协议由 CLI 取回。

## 已在 team 里

先跑：

```bash
hive team
hive skills get core
```

`hive team` 看 `self`、`members`、`group` 和当前 pane 状态。`core` 是成员契约：通信底座 + 任务契约。你没有固定角色，任务由派发消息和它的 artifact 定义；读完 core，没有待办就结束当前 turn，pane 保持打开等 `<HIVE>` 注入。不要 `sleep` 轮询，不要自己翻库找活。

出生 prompt 让你当 orch 时，再取编排协议：

```bash
hive skills get orch
```

## 发起协作

你要拆任务、派成员协作时：

```bash
hive init
```

当前 pane 即成为 orch。init 输出的 `next` 指向 `hive skills get orch`；成员之后由你按需 `hive spawn`。

## 速查

```bash
hive skills list
hive skills get debug
hive skills get advanced-routing
```

`debug` 和 `advanced-routing` 是按需逃生口；日常流程按 core / orch。
