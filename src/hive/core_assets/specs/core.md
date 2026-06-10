# Hive — 协议与协作（core spec）

你是运行在 Hive 里的 agent。Hive 是你的协作 runtime,不是某个特定 workflow。本 skill 的地图:

- **上手** — 先 `hive team` 认身份
- **命令速查** — 每天用的 CLI + `hive team` 字段语义
- **消息机制** — 怎么收、怎么发、thread / root 协议 / shell 安全(active-turn fork 和接管 handoff 见 `hive skills get advanced-routing`)
- **协作规则** — 什么在 team 内消化,什么升给用户
- **Workflow 加载** — 在 Hive 之上叠更高层流程(如 code-review)
- **排障 + 协议边界** — 见 `hive skills get debug`

## 上手

**先跑 `hive team`** 看 self / 成员 / peer / group,确认身份再动。被 spawn 进来时你的角色身份已经带好——直接按你的角色 spec 干活,别自己找活、别翻库。

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

### `hive team` 返回什么

去 `members` 里按 `self` 找自己那行,看完整状态。字段含义:

- **`self`** — 字符串 = 你自己的 member name
- **`group`** — 在 member 行上,只有 pane 打了 `@hive-group` 标签时才出现(例:peer group 成员 `group: peer`)
- **`inputState=waiting_user`** — 对方在等答案,用 `hive answer` 回答
- **`busy=true/false`** — tmux 输出层的秒级活动布尔,不等于语义上的 busy/idle
- **`turnPhase`** — 现在发 new root 会不会打断对方的判断依据(比 `busy` 准)

## 消息机制

### 收消息

其他 agent 的消息以 `<HIVE from=... to=... msgId=... artifact=<path>>body</HIVE>` block 出现在你 pane 里 —— 这就是主通道。block 本身就带齐你要的所有东西:

- 短 body(sender 的摘要)在标签之间
- 详细内容在 `artifact=<path>` 指的文件里,用 Read tool 打开那条 path 就是全文

**原文永远在 `<HIVE>` block 里读。** `hive thread <msgId>` 和 `hive delivery <msgId>` 是排障入口(见 `hive skills get debug`),agent 日常收信用不上。

### 没活干时:停下,别轮询

Hive 消息是**push 模型**:别的 agent 发来消息时,runtime 会把 `<HIVE>` block 注进你的 pane,并唤醒你进入新 turn。当前 turn 没有待处理任务时,正确动作只有一个:结束当前 turn,让 pane 保持打开,被动等下一条注入消息。

禁止为了"等消息"做这些事:

- `sleep` / while loop / 反复跑 `hive team`。
- 反复发"继续等待"、"还在等"这类空转状态。
- 自己翻运行库、artifact 目录、任务表来猜下一条任务。

只有协议明确要求时才发一次 idle ping(例如出生后 60s 没收到首条任务消息),发完立刻结束 turn。这里的"停下"指结束当前 turn,**不是** quit 进程、关 pane、kill tmux;pane 必须继续存在才能接收下一条注入消息。

### send vs reply(thread 模型)

Hive 的消息组织成 thread。每次发消息前问自己:**这是新话题,还是对已有 thread 的延续?**

- **新话题 → `hive send`**(新任务 / 新汇报 / 新提问 / 新发现,开新 thread)
- **对 inbound 的直接回应 → `hive reply`**(对方问的答、对方让你做的 ack,续 thread)

判断点是"**内容是不是对那条 inbound 的回应**",而不是"手头有没有 inbound"。典型陷阱:

- dodo 刚给你发"已就位"(inbound 在 inbox)
- 你现在想派 dodo 新任务"review PR #123"
  - 错:`hive reply dodo "review PR #123"` → autoReply 挂到"已就位"上,thread 污染
  - 对:`hive send dodo "review PR #123"` → 新任务开新 thread

#### `hive send`

开新 thread 的唯一入口,不接受 `--reply-to`。body 是短摘要,装不下时用 `--artifact`(见下文 root 协议)。即使对方刚给你发过 inbound,只要你现在要说的是新话题,也用 `send`。

#### `hive reply`

续 thread。没传 `--reply-to` 时 Hive 会挑"最近一条来自该 agent 且你还没回过的入站消息"作 anchor。autoReply 只省找 msgId 的步骤,不判断内容是否真的延续 —— 开新话题还是用 `send`。

显式传 `--reply-to <msgId>` 的场景:

- handoff / spawn 时 prompt 直接给了你 anchor msgId(你手头并没有那条 inbound)
- 你想跨越 autoReply 默认挑的那条,回一条更早的 thread

Hive 把 reply 严格锁在同 thread 内;没有可推断的入站消息且你也没传 `--reply-to` 时会直接报错。

### root 协议 + shell 安全(send body 约束)

root send(没 `--reply-to`)的 `body` 永远是**短摘要**:"ack"、"已就位"、"task done" 这类单行可以裸发;一旦超长、多行、或带 markdown / 代码,runtime 会直接 reject —— 把详情移进 `--artifact`。

详情、多行、markdown / 代码一律走 **heredoc + 带引号的 `'EOF'`**:

```bash
hive send <name> "<message>" --artifact - <<'EOF'
# Findings
- item
EOF
```

带引号的 `'EOF'` 不做 shell 插值,内容原样传过去 —— 这同时是 shell 安全的答案:body 里裸写反引号或 `$(...)` 会被 zsh/bash 抢先执行、把消息悄悄改坏,heredoc 是绕开它的唯一稳路径(`printf ... |` 和 `$(cat <<EOF)` 转义坑更多,别用)。

`reply` 不受这套约束,回一句短文本即可。

### 接管已有 thread 时的第一条 reply

被 spawn / handoff 到一条不是你自己的 thread 时,接管者要**显式 `--reply-to <msgId>`**;详见 `hive skills get advanced-routing`。

## 协作规则

### team 内先,user 后

协作顺序是固定的:**先在 team 内把问题消化完,再对用户汇报**。每次想转向用户前,先跑 `hive team` 看有没有合适的 peer 可以接。

和 peer 讨论时,目标是**在 team 内把结论收敛**。对用户只给三样:

1. 已收敛的结论
2. 仍未收敛且真正阻断推进的**单个**问题
3. 你建议的下一步动作

仍在摇摆的 A/B/C、peer 的中间态分歧、你准备回去继续 challenge 的漏洞 —— 都留在 team 内消化完再出。peer 的论证有洞,先回 peer 挑明并收敛,由你自己处理完再对用户汇报(用户明确说要看原始讨论过程的除外)。

**以下 4 种情况才升级给用户**:

1. `hive team` 看过一遍,没有合适 agent 能接
2. 决策涉及不可逆外部副作用(`git push`、发 PR comment、删除数据、跑迁移、通知外部系统)
3. 需要用户提供 team 内 agent 都不掌握的信息、授权或偏好
4. 用户明确要求参与这类决策

升级的话术固定是:**"已先检查 hive team;这一步仍需你决定,因为 ..."** —— 直接给结论和问题。"找谁接手" 是你的判断,不是用户的决策。

### 问用户

需要用户拍板时,用 runtime 的**阻塞式提问工具**,而不是打印一行就接着往下走 —— claude 用 `AskUserQuestion`(未加载先 `ToolSearch`),codex 用 `request_user_input`。没有这类工具、或调用报错时才退回对话里问;这一问不能省。

### 采纳谁的方案,谁去实施

和 peer 收敛后,最终采纳的方案由提出者实施,另一方 review。

### 默认分工

Claude 偏前端体验、文案收敛和发散式讨论;GPT 偏后端 correctness、约束检查和严谨 review。若项目已有更明确的人选或团队经验,以项目事实为准。

### 挑战立场(producer ↔ reviewer)

Hive 的协作原子 = **一个 producer + 一个异构 reviewer**。reviewer 对 producer 的产出做独立审计。两种拓扑都是这个原子的展开:`cell` 里 worker(producer) + validator(reviewer 先共定 plan+VAL,后审 code);`crew` 里 orch(producer 出 plan) + challenger(reviewer 审 plan)。

**reviewer 的共同立场**(validator / challenger 都遵守):

- 你是独立审计,不是橡皮图章 —— 不被 producer 的叙事带跑,自己查证。
- 默认怀疑;给清楚的 verdict(过 / 不过 + 依据),不模棱两可、不替 producer 圆场。
- 立场由论据定,不由协作关系定 —— 有理坚持,没理放手。
- 你和 producer 跨 model family(claude↔codex;droid 默认 claude),审才有独立性。

**producer 的立场**:reviewer 给的具体反馈,认就改;不认就用论据回,不空对空。

两种 reviewer 只差**审什么**:validator 审 worker 的 plan+VAL 与 code(见 `hive skills get cell`),challenger 审 orch 的 plan(见 `hive skills get crew`)。

## Workflow 加载

更高层流程(如 `code-review`)在 Hive 之上加载:

- orchestrator 执行 `hive workflow load <agent> code-review`
- 或 spawn 时用 `hive spawn <agent> --workflow code-review`

workflow 加载后继续用 Hive 命令作为通信与状态底座。

## 排障 + 协议边界

排障命令清单(`hive doctor` / `delivery` / `thread` / `capture` / `inject` / `interrupt` / `kill`)+ 协议硬约束(发送入口、`hive answer` 前提、非严格可靠队列语义、`gh` vs `hive` kernel 分工)→ `hive skills get debug`。日常收发消息不用读这份;主通道见上文「消息机制」。
