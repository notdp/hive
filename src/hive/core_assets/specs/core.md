# core — Hive 通信底座（无角色 pane 读这一份）

你是一个被拉进**已有 team** 的 pane，**没有指派角色**——你要的只是和别的 agent 通信的底座。Hive 是 tmux 里的多 agent 协作 runtime，本 skill 给你收发消息、看成员状态、按协作规则升级的全部接口。

本 spec 的地图：

- **被指派了角色的 pane** 读 `hive skills get <role>`（如 `duo-worker` / `duo-validator` / `squad-orch`）——那一份自包含，一次取齐角色协议 + 这里的通信底座，不必再读 core。
- **没有角色的 pane**（就是你）读 core：通信底座 + 协作规则 + 命令速查，够你在 team 里收发、汇报、升级。

第一步：`hive team` 认身份、看成员 / peer / group，记下你的 member name 和能接活的 peer。

---

## 上手

**先跑 `hive team`** 看 self / 成员 / peer / group，确认身份再动。

`self` 是字符串，就是你自己的 member name；去 `members` 里按它找自己那行看完整状态。被拉进来时如果带了上下文就按上下文走；没有明确任务时别自己翻库找活，停下等注入（见「没活时停下」）。

## 命令速查

```bash
hive team                            # 成员 + runtime(inputState/busy/turnPhase) + peer + group;`self` 是字符串,指你自己的 member name
hive send dodo "see attachment" --artifact /tmp/file.md   # 已有现成文件时
hive send dodo "see attachment" --artifact - <<'EOF'
# Findings
- item
EOF
hive reply dodo "ack, looking"       # 回复 dodo 最近一条给你的消息(自动 reply-to)
hive answer claude "yes"             # 回答 agent 的 pending question
```

### `hive team` 字段语义

去 `members` 里按 `self` 找自己那行,看完整状态。字段含义:

- **`self`** — 字符串 = 你自己的 member name
- **`group`** — 在 member 行上,只有 pane 打了 `@hive-group` 标签时才出现(例:peer group 成员 `group: peer`)
- **`inputState=waiting_user`** — 对方在等答案,用 `hive answer` 回答
- **`busy=true/false`** — tmux 输出层的秒级活动布尔,不等于语义上的 busy/idle
- **`turnPhase`** — 现在发 new root 会不会打断对方的判断依据(比 `busy` 准)

---

## 通信底座

### 收消息

其他 agent 的消息以 `<HIVE from=… to=… msgId=… artifact=<path>>body</HIVE>` block 出现在你 pane 里 —— 这就是主通道：

- 短摘要在标签之间；详情在 `artifact=<path>` 指的文件里，用 Read 打开那条 path 就是全文。
- 原文永远在 `<HIVE>` block 里读。`hive thread` / `hive delivery` 是排障入口（`hive skills get debug`），日常收信用不上。

### 发消息：send 还是 reply

每次发消息前问：**这是新话题，还是对某条 inbound 的延续？**

- **新话题 → `hive send`**：新任务 / 新汇报 / 新提问，开新 thread。不接受 `--reply-to`。
- **对 inbound 的直接回应 → `hive reply`**：续 thread，自动锚到“最近一条来自该 agent 且你没回过的”入站消息。

判断点是“内容是不是对那条 inbound 的回应”，不是“手头有没有 inbound”。典型陷阱：validator 刚发你“已就位”，你现在要派新活——用 `send` 开新 thread，别 `reply` 挂到“已就位”上污染 thread。

handoff / spawn 给了你 anchor msgId（你手头并没有那条 inbound）时，显式 `--reply-to <msgId>`；thread 接管细节 `hive skills get advanced-routing`。

### root 消息 + shell 安全

root send（没 `--reply-to`）的 body 永远是**短摘要**。单行的 `ack` / `已就位` / `task done` 可裸发；超长、多行、带 markdown 或代码时，详情走 `--artifact` 的 heredoc：

```bash
hive send <name> "<短摘要>" --artifact - <<'EOF'
# Findings
- item
EOF
```

带引号的 `'EOF'` 不做 shell 插值，内容原样传过去——这同时是 shell 安全：body 里裸写反引号或 `$(…)` 会被 shell 抢先执行、把消息悄悄改坏。heredoc 是稳路径，别用 `printf …|` / `$(cat <<EOF)`。`reply` 不受此约束，回一句短文本即可。

### 没活时停下，别轮询

Hive 是 push 模型：别人发消息时 runtime 自动把 `<HIVE>` block 注进你 pane 并唤醒你。当前 turn 没有待办时，正确动作只有一个：**结束当前 turn**，让 pane 开着被动等下一条注入。

“停下”指结束 turn，**不是** quit 进程 / 关 pane。禁止 `sleep` / while loop / 反复 `hive team` 来“等消息”，也别自己翻库猜下一条活。

---

## 协作规则

### 先 team 内、后对用户

先和 peer 把问题消化完，再带结论找人。对人只给三样：已收敛的结论、仍阻断推进的**单个**问题、你建议的下一步。仍在摇摆的 A/B/C、和 peer 的中间态分歧，都留在 team 内消化完再出。

需要人拍板时用 runtime 的**阻塞式提问工具**，不是打印一行接着往下走：claude 用 `AskUserQuestion`（未加载先 `ToolSearch`），codex 用 `request_user_input`。没有这类工具才退回对话里问，这一问不能省。

### 立场：producer ↔ reviewer

Hive 的协作原子 = **一个 producer + 一个异构 reviewer**。reviewer 对 producer 的产出做独立审计。你被拉进的 team 多半已经在跑这个原子，接活时认清自己当下站哪一边：

- **producer 的立场**：reviewer 给的具体反馈，认就改；不认就用论据回，不空对空。最终采纳谁的方案，谁去实施。
- **reviewer 的立场**：你是独立审计不是橡皮图章——不被 producer 的叙事带跑，关键结论自己从原始证据（artifact / diff / log / command output）算；默认怀疑，给清楚的 verdict（过 / 不过 + 依据），立场由论据定不由协作关系定。

两边跨 model family（claude↔codex；droid 默认 claude），审才有独立性。

### 共享 checkout 纪律

多 agent 在同一 cwd 工作时，git 暂存区 / stash / 当前分支会互相影响。路径含 `.claude/worktrees/`、Hive shared checkout、或多人同 cwd 时，动 git 前先看事实：

- commit 前看 `git status --short` + `git diff --cached --stat`；staged 里有别人或越 scope 文件，先和 owner 收敛，别卷进自己的 commit。
- stash 前看 `git stash list`；不 pop 别人的 stash，不静默 stash 别人的 untracked 文件。

### 默认分工

claude 偏前端体验、文案收敛和发散式讨论；GPT 偏后端 correctness、约束检查和严谨 review。若项目已有更明确的人选或团队经验，以项目事实为准。

### Human Directive

human 的直接指令可出现在任何 artifact / message body 里，格式 `humanDirective: "原文引用"` + `source: <来源>`。识别这个字段：已授权 scope 的变更不必再走 gate；转发时保留原文和 source 不改写。

---

## Workflow 加载

更高层流程（如 `code-review`）在 Hive 之上加载：

- orchestrator 执行 `hive workflow load <agent> code-review`
- 或 spawn 时用 `hive spawn <agent> --workflow code-review`

workflow 加载后继续用 Hive 命令作为通信与状态底座。

## 排障 + 协议边界

排障时按需取 `hive skills get debug`：覆盖 `hive doctor` / `delivery` / `thread` / `capture` / `inject` / `interrupt` / `kill`、`hive answer` 前提、队列语义、`gh` vs `hive` kernel 分工。

active-turn fork 和接管 handoff 的 thread 细节按需取 `hive skills get advanced-routing`。

日常收发消息不用读这两份；主通道见上文「通信底座」。
