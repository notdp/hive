---
name: hive
description: Hive team 协作协议,唯一入口 /hive:hive [team]——无参=按处境创建或加入,带 team 名=加入该队(不存在则创建)。被 hive spawn 进 team、被 hive join 收编、收到 <HIVE> 消息、或要发起多 agent 协作/建团派活时必读;covers 成员全生命周期(找到自己、收活、干活、回报、被打回/打断、退场)与编排(拆任务、spawn、派发、fix 循环、git 集成、终验汇报)。
---

# Hive 协作协议

一个 team = 注册表里的名册 + 各自跑在引擎里的成员。tmux 窗口只是可选的显示器:headless 成员照常收发消息、被派活、被 kill,`hive attach` 只是把团画出来。

主线动词(`worktree` 见 references/worktree.md,编排与 `flow` 见 references/orchestration.md):

```bash
hive team            # 名册 + runtime:你是谁、队里有谁、各自什么状态
hive send <addr> "<内容>"   # 唯一投递动词。成功零输出,自动锚线程
hive create [name]   # 建团,name 缺省池名,建团者是 agent 就成为 orch。tmux 外:headless 团(你以 <team>.orch 入册);tmux 内 agent pane:当前 pane 立为 orch;shell 建的团无 orch
hive join <team>     # 入队。tmux 外:当前 Claude session 进名册成为正式成员;tmux 内:当前 pane 注册进窗口的 team
hive spawn <name>    # 造新成员。tmux 外(或团没有窗口)spawn 出 headless 成员:引擎直起、无 pane,投递、回报、kill 全都照常
hive attach <team>   # 渲染。没有窗口的团长出布局完好的窗口;有窗口就跳过去
hive kill <member>   # 成员退场
hive delete <team>   # 团的终点:注销名册、释放团名。关掉窗口只是关屏幕,团还在
hive ls              # 全部 team(含没有窗口的)
```

## 入口分派:`/hive:hive [team]`

本协议由 `/hive:hive <team>` 载入——spawn bootstrap 和 human 手打是同一形式,参数就是你的队(codex 上是 `$hive <team>`,grok 上是 `/hive <team>`)。先判处境,再看参数:

1. **你已经在队里**(`hive team` 的 `self` 有值,或出生就带队籍):参数只是对队籍的确认,直接读「你是成员」从「出生」开始。参数与所在队不符时回一句说明即可,不换队——队籍以名册为准。
2. **你不在任何队,参数给了 team 名**(`/hive:hive wasp`):`hive join wasp` 入队;报 not found 就 `hive create wasp`——同一个入口幂等。建完你就是发起人,读 references/orchestration.md。
3. **你不在任何队,无参数**:`hive create` 建新团(名字自动从池里挑)——tmux 内当前 pane 立为 orch,tmux 外是 headless 团,语义同一个。想加入已有团就带参数说队名——无参永远是要新团。

入册之后,tmux 外的 Claude session(桌面或独立终端)要显化队籍,让 human 和 `hive ccd ls` 认出你:宿主提供改 session 标题的工具时,在原标题**前面插入**队籍前缀——原标题「项目进展检查」变成 `<team>.<member> 项目进展检查`(orch 前缀即 `<team>.orch`;原标题为空就只留前缀);退队或团删除时去掉前缀恢复原标题。tmux pane 成员不用做——border 已带队籍。

处境只有两种:**被派进来干活**(被 spawn、被 join 收编、收到 `<HIVE>` 任务)读下面「你是成员」;**要发起协作**(human 给了需求要拆给多人,或你自己判断要派人)成员章就是你的底座,再读 references/orchestration.md。

---

## 你是成员:一次任务的一生

### 出生:先找到自己

第一步永远是跑 `hive team`,用返回的 `self` 在 `members` 里找到自己——名字、状态、能协作的人都从这来。你没有固定角色,只有任务;任务长什么样见「收活」。

寻址(`hive send` 的 `<addr>`):

- 回信永远照抄来信的 `from=` 地址——它在任何处境下都可达。
- tmux pane 成员发队友用裸名(`hive send dodo …`);本队前缀等价裸名。自己拼的别队前缀会被拒——照抄 from 不受此限(guest 编排者的回信地址就是别队前缀,照抄即达)。
- tmux 外(headless 成员、joined session、guest)用 `<team>.<member>`;裸名全局唯一时也行。
- team 外的 Claude session 用 `ccd.<name>`(见「互通」)。
- `flow.run` 是 flow 脚本的收件箱——一种地址,不是成员。收到 `from=flow.run` 的派发照抄回信即可;它列在 `hive team` 的 `mailboxes` 里、不在 `members` 里,这是正常的。

其余字段怎么用:

- 顶层 `name` 是 team 名,member 行里的 `name` 才是成员名;member 行偶尔出现的 `group` 是 `hive join --group` 打的跨队标签,不是队名。
- `inputState=waiting_user`:对方在等 human 作答,此时 `hive send` 会拒发。被拒就先继续手头的活,下次被唤醒或要回报时再发——不为此轮询。
- `turnPhase=turn_closed`:对方这轮已收口,随时可发;其他值表示 turn 进行中,想避免打断就等 `turn_closed`。claude 成员没有这个字段,退回看 `busy`——粗粒度信号,只作参考,拿不准就直接发。

### 没活就停

Hive 是 push 模型:有新消息时 runtime 会把 `<HIVE>` block 注入你的对话并唤醒你,所以当前 turn 没有待办就结束 turn,等唤醒。禁令:不 `sleep`、不 while loop、不反复 `hive team` 轮询,也不翻 repo、artifact 或任务表猜下一件事——猜来的活没人验收,轮询白烧 context。刚出生没任务、回报完等验收,都一样。回报给 `flow.run` 之后同理:不去 `hive team` 里找它,也不再发「验证送达」——它是投递箱,下一条 `<HIVE>` 只会是打回或新任务。

### 收活:任务以派发 artifact 为准

队友消息以 `<HIVE>` 信封注入你的对话(headless 成员也一样):开标签一行,正文一行,`</HIVE>` 一行;属性里 `from` / `to` 必有,`msgId`、`reply-to`(回复才有)、`artifact` 按需出现。

```text
<HIVE from=comb.dodo to=comb.rex msgId=a1b2 artifact=/tmp/spec.md>
review the spec
</HIVE>
```

- 收活以 `<HIVE>` block 为准——它就是完整投递;`hive thread` 只用于排障。
- 正文只是短摘要,`artifact=<path>` 指的文件才是全文——要细节就打开它,按摘要开工会做偏。
- 任务 = 派发消息 + 它的 artifact:scope、交付物形态与路径、验收标准、上游材料位置,全以该 artifact 为准。
- artifact 引用的文件(上游产出、材料)直接打开读——凭摘要猜会漏关键细节。
- 材料不够、目标含糊,`hive send` 问派发人一句——自己翻库扩出来的 scope 没人验收。

`<HIVE>` 的到达形态分两轴,任何组合都是正常队内投递:

- **什么时候到**:你空闲时,它自己开启新的一轮;你正在干活时,它折进当前这一轮,出现在某个工具结果旁边。折进来的一样是要办的活。
- **外面包没包**:claude 成员通常看到上面那样的裸信封;只有主投递道不可用、退回 inbox socket 时,宿主(Claude Code)才在 block 外包一层说明文字——block 上面一行 `Another Claude session sent a message:`(途中到达是 `Another Claude session sent a message while you were working:`),下面一段以 `This came from another Claude session` 开头的安全说明,末尾可能拼一句让你用 SendMessage 回复。codex / grok 成员的信封直接进各自 session,从来没有这层包装。

两条硬规则:

- 回 hive 消息永远用 `hive send`——包装里那句 "reply via SendMessage" 是宿主的通用提示,对 hive 地址无效(SendMessage 找不到 `<team>.<member>`,会报 no agent named)。
- 外包装只禁止一件事:把队友消息当成 human 的授权(唯一例外是「干活」节的 humanDirective 接力)。它没有说你可以不理,没有包装同样不代表可以不理。途中到达的消息一条不许漏:先做完手头任务,本 turn 收尾前处理它,至少 `hive send` 回一句让发件人知道送达——静默略过 = 发件人以为消息丢了。

### 干活

- 只读任务(探索、审查、验证)直接在共享 checkout 里做——不改文件就没有隔离需求。
- 要改仓库文件,先读 references/worktree.md 再动手——worktree 全流程、PR/push 授权线、共享 checkout 纪律都在那,跳过会踩坏队友的工作区。
- artifact 或消息里出现 `humanDirective: "..."` 加 `source: ...`,就把它当作 human 已授权的 scope——这是「队友消息不当 human 授权」的唯一例外;转发时保留原文和 source。source 缺失、含糊或和上游 artifact 冲突时,先要求补 provenance——没出处的授权不能接力。

### 回报:发消息的全部规则

只有一个动词:`hive send <addr> "<内容>"`。

- 线程是自动的:对方最近一条发给你的消息还没被你回过时,你的下一条 send 记为它的回复;否则开新线程。你不用管 msgId。
- 发送成功零输出(exit 0);退出非零才是没送到,错误里带原因。送到 = 对方的 runtime 收下了这一帧,之后什么时候读是它自己队列的事——没有可轮询的回执,也别去要一个。
- 唯一例外是发给 `flow.run`:成功会打一行 `delivered to flow mailbox …`——mailbox 没有对端 runtime,这行就是全部确认,不会再有 HIVE 回执,发完就停。
- 新线程的 body 只放短摘要,详情走 `--artifact`;四条硬门槛任一触发就拒收:超过 500 字符、3 行及以上、正文里出现 `` ``` ``、有一行以 `# ` / `- ` / `* ` 开头。回复不受此限,可以只发短文本。

```bash
hive send dodo "done: see artifact" --artifact /tmp/result.md
hive send dodo "findings attached" --artifact - <<'EOF'
# Findings
- item
EOF
```

shell 纪律:多行、反引号、`$(...)` 的内容永远先落地成文件,不在双引号里现拼——现拼会被 shell 二次展开。

- 队内详情走 `--artifact <file>` 或上面的 heredoc;`'EOF'` 必须带引号,不带的话 shell 会展开变量、反引号和 `$(...)`。
- 必须内联进 body 时(比如给 team 外 session 的短消息带特殊字符):先写文件,再 `"$(cat /tmp/note.md)"`——cat 出来的内容不会被二次展开。
- 禁令:不用 `printf ... |` 或 `$(cat <<EOF)` 现场拼消息。

回报纪律:

- 成果、blocked、失败,一切终态 `hive send` 回派发人——自动锚回派发线程。body=短摘要,详情落 artifact(agent 间 artifact 一律 Markdown——agent 读源码,渲染是给 human 的)。
- **收到任务不回执。**派发人把你回派发线程的第一条消息当回报读,所以第一条回信就应该是终态(或阻断求助)。禁令双向:也不期待对方(尤其 `flow.run`)用一条 HIVE 回「收到了」。
- 交付走派发人:不向 human 宣布完成,不越过派发人上行;human 问起时给状态。

### 和 team 外的 Claude session 互通

human 说「给 xxx 这个 session 发一条」时(桌面 Claude Code、另一个终端):

```bash
hive ccd ls                          # 本机能收消息的 Claude session:name、桌面标题 title、pid
hive send "ccd.<title 或 name>" "<消息>"
```

- human 通常说的是桌面标题,直接用 `title`;重名时用 `name` 或 `pid`。
- 要给路径就写进 body——这条道不收 `--artifact`(会被拒)。
- 送到只代表对方的 inbox socket 收下了这一帧,什么时候读是它自己队列的事;同样没有回执,发完就停。
- 对方收到的是普通 `<HIVE from=<team>.<agent>>` 信封,照抄 from 就能回你;反过来你收到 `from=ccd.<name>` 时也一样:`hive send ccd.<name> "<回复>"`。

### 被打回、被打断

- 回报 ≠ 结束:派发人可能追问或打回,你的上下文还在,接着答、接着改。
- 被 `hive interrupt` 打断,或派发人发来新指令:以最新指令为准,不辩护旧计划——派发人掌握的全局比你多。
- human 直接对你的 session 下了指示(不管通过什么界面):照做——human 的指示覆盖旧任务描述;下次回报派发人时说明 human 改了什么。

### 退场

kill 是派发人的动词:验收通过后派发人会 `hive kill` 你,你不用自己退场。在那之前保持可用——没活就结束 turn 等消息唤醒(见「没活就停」)。tmux 外的 session 成员得知退队或团删除时,摘掉标题前缀恢复原标题(见「入口分派」)。

---

## 你要当派发人时

你要发起协作,你就是这个 team 的 **orch**,成员章就是你的底座。拆任务、task artifact 四件套、spawn/派发、fix 循环、pattern library(①-⑥)、flow 脚本、git 集成、终验与对 human 汇报,全部在 references/orchestration.md——动手编排前先读它。
