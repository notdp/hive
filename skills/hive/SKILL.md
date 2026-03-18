---
name: hive
description: Hive 基础 skill。让 agent 作为 Hive runtime 成员工作：发现上下文、查看成员、接收 `<HIVE ...>` 消息、发布状态、发送消息，并加载更高层 workflow skill。
metadata: {"hive-bot":{"emoji":"💬","os":["darwin","linux"],"requires":{"bins":["tmux","droid","python3","hive"]}}}
---

# Hive - agent 通信基础层

Hive CLI 是必须的外部依赖。安装方式：

```bash
pipx install git+https://github.com/notdp/hive.git
```

运行 `hive --help` 确认安装成功。

---

你是运行在 Hive 里的 agent。Hive 是你的协作 runtime，不是某个特定 workflow。

## 目标

1. 独立使用 Hive 基础能力
2. 先确定上下文，再执行消息/协作命令
3. 必要时再加载更高层 workflow skill（例如 `cross-review`）

## 先确定上下文

优先顺序：

1. 直接运行 `hive current`
2. 如果已有 `team/workspace/agent`，继续用 Hive 命令
3. 如果没有 team 但在 tmux 里：
   - `hive current` 会显示检测到的 tmux 环境（session、window、pane 列表）
   - 运行 `hive init` 一键从当前 tmux window 创建 team + workspace，自动注册其他 pane 为 agent 并推送 `/skill hive`
4. 如果已有 team 但当前 session 未绑定：
   - 先运行 `hive teams` 看有哪些 team
   - 再运行 `hive use <team> --agent <your-name>` 绑定当前 session

## 命令速查（注意参数都是 positional，不要加 --to / --msg）

```bash
hive current                          # 当前上下文（无 team 时自动发现 tmux）
hive init                             # 从 tmux window 创建 team，自动注册所有 pane
hive who                              # 查看成员
hive send claude "hello"              # 发消息（第1个参数=收件人，第2个=内容）
hive send claude "see attachment" --artifact /tmp/file.md
hive status-set busy "working on X"   # 发布状态
hive status-show                      # 查看所有人状态
hive teams                            # 列出已知 team
hive use <team> --agent <name>        # 手动绑定上下文
```

## 工作原则

1. 先 `hive current`（无 team 时跑 `hive init`）
2. 再 `hive who`
3. 其他 agent 发来的消息会直接以 `<HIVE from=... to=... [artifact=...]> ... </HIVE>` 形式出现在当前 pane
4. 发消息用 `hive send <name> "<message>"`（positional，不要用 --to）
5. 大内容写 artifact，再把路径通过 `hive send <name> "see artifact" --artifact <path>` 发出去
6. 开始任务时主动 `hive status-set busy ...`
7. 完成时 `hive status-set done ... --meta artifact=<path>`

## 协议边界

- `hive send` 是 tmux 内联消息注入，适合发任务、澄清、blocker、请求对方采取下一步动作
- `hive status-set` / `hive status-show` / `hive wait-status` 是控制面状态快照，适合表示 `busy/done/fail`、阶段、artifact 路径等
- 对 workflow 来说，**完成态默认用 `status + artifact` 回传，不要再机械发送 `hive send <orchestrator> "... complete"`**
- GitHub PR comment / review 属于 workflow 层职责；需要发评论时直接用 `gh` / `gh api`，不要把这类 API 混进 Hive kernel 命令
- [推断] `hive send` 不是严格可靠消息队列：没有送达确认、幂等性或 backpressure；需要可轮询、可恢复的完成信号时，应依赖 `status` 和 workspace artifact

## 加载 workflow

Hive 是基础 skill。更高层流程应在 Hive 之上加载，例如：

- 由 orchestrator 执行 `hive workflow load <agent> cross-review`
- 或在 spawn 时使用 `hive spawn <agent> --workflow cross-review`

当 workflow skill 被加载后，继续使用 Hive 命令作为通信与状态底座。
