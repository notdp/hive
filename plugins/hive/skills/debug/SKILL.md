---
name: debug
description: Hive 排障命令清单与协议硬边界。doctor/thread/capture/inject/interrupt/kill 与日志位置；日常收发不用读。
---

# Hive debug + 协议边界

排障命令清单和 hive kernel 的协议硬约束。主通道见 `/hive:hive`;日常收发消息不读这份。

## Debug / 排障

- `hive doctor [agent]` — agent 连通性
- `hive thread <msgId>` — 某条消息的 reply / observation 串联
- `hive capture / inject / interrupt / kill` — 低层 pane 操作

### 日志位置

`hive doctor` 默认输出当前 workspace 的排障路径,不需要额外 flag:

- `runDir` — workspace 的运行时目录
- `logs.notify` — notify / idle watcher JSONL
- `logs.hived_stderr` — hived 未捕获异常和 stderr 兜底
- `logs.cvim_dir` — `hive cvim` / `hive vim` 每次调用的 per-run JSONL

`normal` 只过滤 hived 心跳类事件,notify / cvim 的业务关键路径仍全量记录。复现日志问题时用逃生口 `HIVE_LOG_VERBOSITY=dev|normal` 临时切换。

## 协议边界

- `hive send` 是唯一消息动词;对方最近一条未回的入站消息存在时自动续该 thread(带 `in_reply_to`),否则开新 root
- Hive 不是严格可靠消息队列;没有幂等性或 backpressure。
- 收件箱一律是 pane 内联的 `<HIVE ...>` block。
- 排障用 `hive thread`,不要把内部存储或日志当收件箱轮询。
- GitHub PR comment / review 属于 workflow 层职责,直接用 `gh` / `gh api`;Hive kernel 命令保持单一职责
