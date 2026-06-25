---
name: hive
description: Hive 是 tmux 里的多 agent 协作 runtime。收到 HIVE 消息、被拉进 team、或要和别的 agent 协作时，用它确认身份、取当前角色协议、收发消息。
---

# Hive — agent 协作入口

Hive 让多个 agent 在 tmux 里用 `<HIVE ...>` 消息协作。这个文件只是发现入口；真正协议由 CLI 按当前角色取回。

## 已在 team 里

先跑：

```bash
hive team
```

看 `self`、`members`、`group`、`peer` 和当前 pane 状态。

如果出生 prompt 或 init 输出给了角色，就只取那一份：

```bash
hive skills get duo-worker
hive skills get duo-validator
hive skills get squad-orch
hive skills get squad-challenger
hive skills get squad-worker
hive skills get squad-validator
```

这些角色 spec 都是自包含协议。取回一份、读完、照它做；不要再拼别的 role spec。

如果你只是被拉进已有 team、没有角色，取：

```bash
hive skills get core
```

没有待办时结束当前 turn，pane 保持打开等下一条 `<HIVE>` 注入。不要 `sleep` 轮询，不要自己翻库找活。

## 开新拓扑

你要新开协作时，先用阻塞式提问工具问 human 要 **duo** 还是 **squad**。不要替 human 猜。

- duo：你和一个异构 validator 闭环一件事。
- squad：orch 拆多 feature，challenger 审 plan，再按需派多个 duo。

按答案跑：

```bash
hive duo init
hive squad init
```

init 的 JSON 会给 `next`，例如 `hive skills get duo-worker` 或 `hive skills get squad-orch`。你自己跑 `next` 取当前 pane 的完整协议。

## 速查

```bash
hive skills list
hive skills get debug
hive skills get advanced-routing
```

`debug` 和 `advanced-routing` 是按需逃生口；日常流程按当前角色 spec。
