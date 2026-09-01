---
name: hive
description: Hive team 协作协议,唯一入口 /hive:hive [team]——无参=按处境创建或加入,带 team 名=加入该队(不存在则创建)。被 hive spawn 进 team、被 hive join 收编、收到 <HIVE> 消息、或要发起多 agent 协作/建团派活时必读;covers 成员全生命周期(找到自己、收活、干活、回报、被打回/打断、退场)与编排(拆任务、spawn、派发、fix 循环、git 集成、终验汇报)。
---

# Hive 协作协议

一个 team = 注册表里的名册 + 各自跑在引擎里的成员。tmux 窗口只是可选的显示器：headless 成员照常收发消息、被派活、被 kill，`hive attach` 只是把团画出来。

主线动词（`worktree` / `ccd` / `thread` / `flow` 在各自小节里按需登场）：

```bash
hive team            # 名册 + runtime：你是谁、队里有谁、各自什么状态
hive send <addr> "<内容>"   # 唯一投递动词。成功零输出，自动锚线程
hive create [name]   # 建团,name 缺省池名,建团者是 agent 就成为 orch。tmux 外：headless 团（你以 <team>.orch 入册）；tmux 内 agent pane：当前 pane 立为 orch；shell 建的团无 orch
hive join <team>     # 入队。tmux 外：当前 Claude session 进名册成为正式成员；tmux 内：当前 pane 注册进窗口的 team
hive spawn <name>    # 造新成员。tmux 外（或团没有窗口）spawn 出 headless 成员：引擎直起、无 pane，投递、回报、kill 全都照常
hive attach <team>   # 渲染。没有窗口的团长出布局完好的窗口；有窗口就跳过去
hive kill <member>   # 成员退场
hive delete <team>   # 团的终点：注销名册、释放团名。关掉窗口只是关屏幕，团还在
hive ls              # 全部 team（含没有窗口的）
```

## 入口分派：`/hive:hive [team]`

本协议由 `/hive:hive <team>` 载入——spawn bootstrap 和 human 手打用的是同一个形式，参数就是你的队（同一个 skill 在 codex 上是 `$hive <team>`，在 grok 上是 `/hive <team>`）。先判处境，再看参数：

1. **你已经在队里**（`hive team` 返回的 `self` 有值，或你出生时就带着队籍）：参数就是对你队籍的确认，直接进「你是成员」一章，从「出生」开始。参数与所在队不符时，回一句说明即可，不换队。
2. **你不在任何队，参数给了 team 名**（`/hive:hive wasp`）：`hive join wasp` 入队；报 not found 就 `hive create wasp` 建团——同一个入口幂等，建完你就是发起人，读「你要当派发人时」。
3. **你不在任何队，无参数**：`hive create` 建新团（名字自动从池里挑）——tmux 内当前 pane 立为 orch，tmux 外是 headless 团，语义同一个。想加入已有团就带参数说队名，无参永远是要新团。

入册之后，tmux 外的 Claude session（桌面或独立终端）要显化队籍：宿主提供修改 session 标题的工具时，把标题设为 `<team>.<member>`（orch 即 `<team>.orch`），human 和 `hive ccd ls` 由此识别你；退队或团删除时改回原标题。tmux pane 成员不用做——border 已带队籍。

处境只有两种，全文按此分章：**被派进来干活**（被 spawn、被 join 收编、收到 `<HIVE>` 任务）读「你是成员」；**要发起协作**（human 给了需求要拆给多人，或你自己判断要派人）成员章就是你的底座，再读「你要当派发人时」。

---

## 你是成员：一次任务的一生

### 出生：先找到自己

第一步永远是跑 `hive team`。用返回的 `self` 在 `members` 里找到自己，确认 member name、当前状态、能协作的人。你没有固定角色，只有任务；任务长什么样见「收活」。

字段怎么用：

- `self`：你的名字。寻址规则：
  - 回信永远照抄来信的 `from=` 地址。
  - 你是 tmux 里的 pane 成员：发队友用裸名（`hive send dodo …`）；本队前缀等价裸名（所以照抄 from 永远安全），别队前缀会被拒。
  - 你在 tmux 外（headless 成员、joined session、guest）：用 `<team>.<member>`；裸名全局唯一时也行。
  - team 外的 Claude session：`ccd.<name>`（见「互通」小节）。
  - flow 脚本的收件箱：`flow.run`——一种地址,不是成员。收到 `from=flow.run` 的派发照抄回信即可;它列在 `hive team` 的 `mailboxes` 里、不在 `members` 里,这是正常的。
- 顶层 `name`：你所在 team 的名字（member 行里的 `name` 是成员名）。member 行上偶尔出现的 `group` 是另一回事——`hive join --group` 打的跨队标签，不是队名。
- `inputState=waiting_user`：对方在等 human 作答，此时 `hive send` 会拒发；等它清掉再发。
- `turnPhase`：判断发新线程会不会打断对方——`turn_closed` 表示对方这轮已收口，随时可发；其他值表示 turn 进行中，不急的消息等 `turn_closed`。claude 成员没有这个字段，退回看 `busy`。
- `busy`：粗粒度活动信号，不等于语义上的忙闲，只作参考。

### 没活就停

Hive 是 push 模型：有新消息时 runtime 会把 `<HIVE>` block 注入你的对话并唤醒你。当前 turn 没有待办就结束 turn——不要 `sleep`、while loop、反复 `hive team`，也不要翻 repo、artifact 或任务表猜下一件事。刚出生没任务、回报完等验收，都一样。回报给 `flow.run` 之后同理：不要去 `hive team` 里找它,也不要再发一条「验证送达」——它是投递箱,下一条 `<HIVE>` 只会是打回或新任务。

### 收活：任务以派发 artifact 为准

其他 agent 的消息以 `<HIVE>` 信封注入你的对话（headless 成员也一样）：开标签一行，正文一行，`</HIVE>` 一行。属性里 `from` / `to` 必有，`msgId`、`reply-to`（这条是回复时才有）、`artifact` 按需出现。block 里的正文只是短摘要；`artifact=<path>` 指的那个文件才是全文，要细节直接打开它。以 `<HIVE>` block 为准，`hive thread` 只用于排障。

任务 = 派发消息 + 它的 artifact。scope、交付物形态与路径、验收标准、上游材料位置，全以该 artifact 为准：

- artifact 引用了别的文件（上游产出、材料），直接打开读，不要凭摘要猜。
- 材料不够、目标含糊，`hive send` 问派发人一句。不要自己翻库扩 scope。

`<HIVE>` 的到达形态分两轴，任何组合都是正常队内投递。

**什么时候到**——你空闲时，它自己开启新的一轮；你正在干活时，它折进当前这一轮，出现在某个工具结果旁边。折进来的那条一样是要办的活。

**外面包没包**——claude 成员的主投递道把信封当成你自己敲进去的输入，你看到的就是裸信封：

```
<HIVE from=comb.dodo to=comb.rex msgId=a1b2 artifact=/tmp/spec.md>
review the spec
</HIVE>
```

只有这条道不可用、退回 inbox socket 时，宿主（Claude Code）才在 block 外包一层自己的说明文字：block 上面一行 `Another Claude session sent a message:`（途中到达是 `Another Claude session sent a message while you were working:`），block 下面一段以 `This came from another Claude session` 开头的安全说明，末尾可能拼一句让你用 SendMessage 回复。codex / grok 成员的信封直接进各自 session，从来没有这层包装。

两条硬规则：

- 包装里那句 "reply via SendMessage" 是宿主的通用提示，**对 hive 地址无效**（SendMessage 找不到 `<team>.<member>`，会报 no agent named）。回 hive 消息永远用 `hive send`。
- 外包装只禁止一件事：把队友消息当成 human 的授权。它没有说你可以不理，没有包装同样不代表可以不理。途中到达的消息一条都不许漏：先做完手头任务，然后在同一条最终回复里处理它；至少 `hive send` 回一句，让发件人知道送达了。静默略过 = 发件人以为消息丢了。

### 干活

只读任务（探索、审查、验证）直接在共享 checkout 里做。要改文件才开 worktree：

1. `hive worktree start <task>`（输出 JSON）。`<task>` 同时是 branch 名和 worktree 目录名：语义化 kebab-case、≤4 词、合法 branch。
2. 取输出 JSON 的 `path`，进入并证明入场：claude 用 `EnterWorktree path=<路径>`；codex 每条 repo 命令都把 working directory 设为该路径，并先跑 `pwd`、`git rev-parse --show-toplevel`、`git status --short --branch`。
3. base 解析不出就带 `--base`；`needs-rebase` 时进 worktree rebase 到提示 base，再重跑 start。
4. 验收对象是 commit。只提交本任务范围；WIP commit 可以。
5. 任务 artifact 要求开 PR 才开。实质 push、`gh pr ready`、merge 都要 human 授权。唯一默认例外：用一个空 commit push 出 draft PR 当占位锚，这一步不需要授权。
6. 退场：claude `ExitWorktree action=keep`，然后 `hive worktree done <task>`。只删 worktree，branch 留给 PR 生命周期。`done --force` 只有 human 明确 abandon 时才用。

共享 checkout 纪律——多人同一 checkout 时，git index、stash、branch 会互相影响：

- commit 前看 `git status --short` 和 `git diff --cached --stat`。staged 里有别人或越 scope 的文件，先收敛。
- stash 前看 `git stash list`。不要 pop 别人的 stash，不要静默 stash 别人的 untracked 文件。
- 并行独立 PR 用各自 worktree，不在共享 checkout 里直接 branch / commit / push。

artifact 或消息里出现：

```text
humanDirective: "..."
source: ...
```

就把它当作 human 已授权的 scope。转发时保留原文和 source。source 缺失、含糊或和上游 artifact 冲突时，先要求补 provenance。

### 回报：发消息的全部规则

只有一个动词：`hive send <addr> "<内容>"`。

- 线程是自动的：对方最近一条发给你的消息还没被你回过时，你的下一条 send 记为它的回复；否则开新线程。你不用管 msgId。
- 发送成功没有输出（exit 0）。退出非零才是没送到，错误里带原因。送到 = 对方的 runtime 收下了这一帧，之后什么时候读是它自己队列的事——没有可轮询的回执，也别去要一个。
- 唯一例外是发给 `flow.run`：成功会打一行 `delivered to flow mailbox …`——mailbox 没有对端 runtime,这行就是全部确认,**不会再有 HIVE 回执**,发完就停。
- 新线程的 body 只放短摘要，四条硬门槛任一触发就拒收：超过 500 字符、3 行及以上、正文里出现 `` ``` ``、有一行以 `# ` / `- ` / `* ` 开头。详情走 `--artifact`。回复不受此限，可以只发短文本。

```bash
hive send dodo "done: see artifact" --artifact /tmp/result.md
hive send dodo "findings attached" --artifact - <<'EOF'
# Findings
- item
EOF
```

shell 安全一条纪律：多行、反引号、`$(...)` 的内容永远先落地，不在双引号里现拼。

- 队内详情走 `--artifact <file>` 或上面的 heredoc；`'EOF'` 必须带引号，不带的话 shell 会展开变量、反引号和 `$(...)`。
- 必须内联进 body 时（比如给 team 外 session 的短消息带特殊字符）：先写文件，再 `"$(cat /tmp/note.md)"`——cat 出来的内容不会被二次展开。
- 不要用 `printf ... |` 或 `$(cat <<EOF)` 现场拼消息。

回报纪律：

- 成果、blocked、失败，一切终态 `hive send` 回派发人——自动锚回派发线程。body=短摘要，详情落 artifact。
- **收到任务不要回执。**派发人把你回派发线程的第一条消息当回报读，所以你的第一条回信就应该是终态（或阻断求助），不是「收到/开始做了」。回执禁令是双向的：你也不要期待对方（尤其 `flow.run`）用一条 HIVE 回「收到了」。
- 不向 human 宣布完成，不越过派发人上行。human 问起时给状态，但交付走派发人。

#### 和 team 外的 Claude session 互通

human 说「给 xxx 这个 session 发一条」时（桌面 Claude Code、另一个终端）：

```bash
hive ccd ls                          # 本机能收消息的 Claude session：name、桌面标题 title、pid
hive send "ccd.<title 或 name>" "<消息>"
```

- human 通常说的是桌面标题，直接用 `title`；重名时用 `name` 或 `pid`。
- 这条道不收 `--artifact`（会被拒）：要给路径就写进 body 里。
- 送到只代表对方的 inbox socket 收下了这一帧；对方什么时候读是它自己队列的事。同样没有回执，发完就停。
- 对方收到的是普通 `<HIVE from=<team>.<agent>>` 信封，照抄 from 就能回你。反过来你收到 `from=ccd.<name>` 时也一样：`hive send ccd.<name> "<回复>"`。

### 被打回、被打断

- 回报 ≠ 结束。派发人可能追问或打回，你的上下文还在，接着答、接着改。
- 被 `hive interrupt` 打断，或派发人发来新指令：以最新指令为准，不辩护旧计划。
- human 直接对你的 session 下了指示（不管通过什么界面）：照做——human 的指示覆盖旧任务描述；下次回报派发人时说明 human 改了什么。

### 退场

kill 是派发人的动词，你不用自己退场：验收通过后派发人会 `hive kill` 你。在那之前保持可用——没活就结束 turn 等消息唤醒（见「没活就停」）。

---

## 你要当派发人时

你要发起协作，你就是这个 team 的 **orch**：拆解任务、spawn 成员、派发、收结论、跑集成终验、向 human 汇报。你不写业务代码。orch 只是先开始派活的那个参与者。

启动：还没有 team 就按开头动词表 `hive create`——建团者就是 orch，tmux 内外一样：你以 `<team>.orch` 进名册，成员回你直接寻址 orch（唯一例外：你已经是别团成员时，你不会再入册，本团以 guest 身份编排，回信地址仍是原队的 `<原 team>.<你的成员名>`，成员照抄 from 就能回到你）。成员的回信会注入你的对话并唤醒你，spawn/派发之后照常结束 turn 等推送。然后 `hive team` 确认 `self`，开始拆解。

### 成员名就是任务标签

runtime 没有角色。你 spawn 时起的名字（`explore`、`impl-auth`、`review`）就是这个成员的全部身份：它出现在消息地址和显示层里。用语义化 kebab-case、≤4 词、看名知事。活着的成员集合就是 workflow 现状——环节推进用 spawn/kill 表达，不改名。

### task artifact 四件套

每个任务先写 artifact（`<workspace>/artifacts/tasks/<member>.md`；workspace 路径在 `hive team` 返回的 `runtimeWorkspace` 字段里），必含：

1. **scope**：做什么、不做什么。
2. **交付物**：形态与产出路径（报告写哪、代码交 commit 还是 PR）。
3. **验收标准**：你终验时按什么判。
4. **材料**：上游产出的 artifact 路径。成员是全盲的，不知道 workflow 形状——它需要的一切材料都在这份 artifact 里给路径，不要指望它自己发现。

### spawn + 派发

```bash
hive spawn explore --task <workspace>/artifacts/tasks/explore.md
hive spawn impl-auth --cli codex --task <workspace>/artifacts/tasks/impl-auth.md
```

- `--task` 把任务作为首条 `<HIVE>` 消息原子投递——成员不会空 inbox 出生。
- `--cli` 缺省跟你同 CLI（claude|codex|grok）；headless spawn 没有 pane 可参照，缺省是 claude。要异构时必须显式传，见 pattern ①。
- model 不确定就别传，默认就是对的（不要照抄状态栏之类的显示串，那不是 model id）。要传时：claude 用别名 `fable` / `opus` / `sonnet`（别名永远指向该档当前最新，不会过期；典型分工：`fable` 做终验/裁决，`opus` 做执行主力）；codex/grok 传具体 id，spawn 按该 CLI 自己的 catalog 校验，打错会带 did-you-mean 拒收。

成员完工会 `hive send` 回报你——自动锚回派发线程。读摘要，必要时读它的 artifact。

### 进度只来自回信

成员的进度信号只有三个：它的回报消息、notify 事件、`hive team` 的 runtime 字段。前两个是推送；`hive team` 用于收到消息后核对状态，不是轮询工具。**不要用 `tmux capture-pane` 或任何读屏手段观察成员**——屏幕是给 human 看的显示层，有残屏和中间态，不是真相；读屏还烧掉你自己的 context。**已派发的任务也不要自己并行做一遍**——你的产出没人验收，还烧掉终验要用的 context。派发出去之后没有待办就结束 turn，等消息唤醒。

### 成员生命周期

- **验收前不 kill**。回报 ≠ 验收：不满意就 `hive send` 打回追问——活成员带全部上下文，杀掉重 spawn 会丢掉上下文。
- 验收通过、下游任务的 artifact 写好之后，`hive kill <member>`，再 spawn 下一环节。
- 唯一例外是 fix 循环（pattern ④）：产出还会被打回的成员，留到下游 verify pass 再 kill。

### Pattern library

以下是建议模式，不是流水线。按任务自由组合；stage 划分、数量、顺序全是你当时的编排决定。

**① producer + 异构 reviewer**——改动需要独立审时，producer 和 reviewer 用不同家族的 CLI——**`--cli` 必须显式写**，忘了就是同构 review，白审：

```bash
hive spawn impl --task <workspace>/artifacts/tasks/impl.md
hive spawn review --cli codex --task <workspace>/artifacts/tasks/review.md
```

review 的 task artifact 里要求 verdict：`pass`/`fail` + evidence + required-changes。reviewer 独立审计，不照抄 producer 叙事；关键结论从 diff、日志、命令输出自己核。

**② solo 快任务**——一个成员闭环一件小事。spawn → 回报 → 验收 → kill。

**③ explore → impl 接力**：

```text
spawn explore ──> 回报(摘要+findings artifact) ──> 验收 ──> kill explore
                          impl 的 task artifact 引用 findings 路径
                                  ──> spawn impl
```

接力棒是 artifact 文件，不是活成员。你只过目摘要，不搬运正文。

**④ fix 循环**——impl 回报后**不 kill**，spawn verify（task 带验收标准 + branch + impl 报告路径）：

- verify fail → `hive send` 把 required-changes 打回给 impl（同成员带上下文修，比新成员快）；verify 也留着，复验时它记得上次挂在哪。
- verify pass → kill impl + verify。
- 建议 5 轮上限，到限升级 human。

**⑤ 集成验收**——所有任务 DONE 后，你自己跑集成验（拉集成分支、跑测试、核验收标准）。过了才向 human 汇报。终验不外包。

**⑥ flow 脚本（机械流程）**——循环、fan-out、barrier 这类确定性控制流不用手工编排：写一个 Python 脚本交给 `hive flow run`，每个 `agent()` 都是真实成员，human 全程可见可介入。`agent()` 走的是 pane spawn，所以这条只在 tmux 里跑得起来，headless 团用不了。

```python
# workflow.py
from hive.flow import agent, parallel

findings = agent("探索认证模块;产出写 <workspace>/artifacts/f.md;完成后回报", name="explore")
a, b = parallel(
    lambda: agent(f"实现 auth,材料见 {findings.artifact};交付 commit", name="impl-auth"),
    lambda: agent("实现 db 层;交付 commit", name="impl-db", cli="codex"),
)
v = agent(f"验证 {a.artifact} {b.artifact};给 pass/fail verdict", name="verify", cli="codex")
if "fail" in v.summary:
    a.ask(f"打回:按 {v.artifact} 的 required-changes 修")   # 同成员带上下文修
```

- 跑法：把 `hive flow run workflow.py` 放进后台 shell，完成后读输出。脚本跑着时你结束当前 turn 等完成通知；期间来了消息照常处理。
- API 全貌（不需要读源码）：
  - `agent(prompt, *, name, cli=None, model="") -> Member`——spawn+原子投递+阻塞等回报。prompt 就是 task artifact，写全四件套。
  - `Member` 字段：`.summary`（回报 body）、`.artifact`（回报 artifact 路径）、`.name`、`.pane`。
  - `member.ask(prompt) -> Member`——追问/打回，阻塞等回答，更新 `.summary`/`.artifact`。
  - `member.kill()`——验收后退场。
  - `parallel(*thunks) -> list`——并发跑，按调用顺序返回；任一失败等全员结束后抛 FlowError。
- 动态判断仍然手工编排；脚本只接机械流程。

### git / 集成纪律

单任务改动直接按成员章「干活」的 worktree 纪律走。多个写码任务并行时，先建集成分支再派发：

```bash
git branch <team>-integration <base>
git push -u origin <team>-integration
hive worktree set-base <team>-integration
```

必须先 set 再 spawn 写码成员：成员的 `hive worktree start` 才会 base 到集成分支。漏 push 时成员开 PR 会报 base 不存在；它会报给你，你补 push。

merge 串行一次一条，只由你做，且在该任务验收通过、human 批准后：

```bash
gh pr merge <PR号> --match-head-commit <验过的head> --squash
```

- 必须带 PR 号，必须带 `--match-head-commit`——避免 pass 后又 push 的 commit 被误合。
- 每合一条，通知 in-flight 写码成员 rebase；它们重跑 start 会拿到 `needs-rebase`。
- 冲突在 PR / 集成点处理；worktree 只隔离工作区，不消除冲突。

首个 sub-PR 合入后可以开 main PR：集成分支 -> main。human review / merge main PR 是最终交付。

### 对 human

- 只给已收敛结论、单个阻断问题、建议下一步。需要拍板用阻塞式提问工具（claude `AskUserQuestion`）。
- 成员越过你直接向 human 交付时，回它「终态发我」。
- human 直接对某个成员改了方向：以 human 为准，更新你手里的验收标准；成员回报时会说明。
- stage 汇报和最终交付要有自包含 HTML。Markdown 源和 HTML 同目录，发 human 的消息给 HTML 绝对路径。agent 间 artifact 一律 Markdown。
- 全部完成且 human 签字后，才 kill 剩余成员；整团收摊用 `hive delete`。

### 窗口相关（可选显示层）

以下命令只在 `hive attach` 出窗口后才有意义，headless 团直接忽略：

- 布局拖乱了跑 `hive layout`。
- PR 号钉窗口状态栏：`hive pr set <PR号>` / `hive pr clear`。