---
name: hive
description: Hive team 协作协议,唯一入口 /hive:hive [team]——无参=按处境创建或加入,带 team 名=加入该队(不存在则创建)。被 hive spawn 进 team、被 hive join 收编、收到 <HIVE> 消息、或要发起多 agent 协作/建团派活时必读;covers 成员全生命周期(找到自己、收活、干活、回报、被打回/打断、退场)与编排(拆任务、spawn、派发、fix 循环、git 集成、终验汇报)。
---

# Hive 协作协议

一个 team = 注册表里的名册 + 各自跑在引擎里的成员。tmux 窗口是显示器,建团即有:窗口被关、tmux 重启都不丢团,`hive attach` 会把它重建出来。

主线动词(`worktree` 见 references/worktree.md,编排见 references/orchestration.md):

```bash
hive team            # 名册 + runtime:你是谁、队里有谁、各自什么状态
hive send <addr> "<内容>"   # 唯一投递动词。成功零输出,自动锚线程
hive create [name]   # 建团,name 缺省池名,建团者是 agent 就成为 orch。tmux 外:建一个以团名命名的 tmux session 放团窗口(Claude session 建团则你以 <team>.orch 入册,窗口首格是你的只读镜像;human 点状态栏 orch 芯片或 `hive mirror` 收起/展开);tmux 内 agent pane:当前 pane 立为 orch;shell 建的团无 orch
hive join <team>     # 入队。tmux 外:当前 Claude session 进名册成为正式成员;tmux 内:当前 pane 注册进窗口的 team
hive spawn <name>    # 造新成员:引擎起在守护进程里,pane 切进团窗口——你在不在 tmux 里都一样
hive attach <team>   # 跳到团的窗口;窗口没了先按名册重建,后来的成员缺 pane 就补上
hive kill <member>   # 成员退场
hive delete <team>   # 注销名册、释放团名;关掉窗口不删团。团目录 $HIVE_HOME/teams/<team>/ 的 bus/run/artifacts 留着,--delete-workspace 才整目录删
hive ls              # 全部 team(含窗口已关的)
```

## 入口分派:`/hive:hive [team]`

本协议由 `/hive:hive <team>` 载入——spawn bootstrap 和 human 手打是同一形式,参数就是你的队(codex 上是 `$hive <team>`,grok 上是 `/hive <team>`)。先判处境,再看参数:

1. **你已经在队里**(`hive team` 的 `self` 有值,或出生就带队籍):参数只是对队籍的确认,直接读「你是成员」从「出生」开始。参数与所在队不符时回一句说明即可,不换队——队籍以名册为准。
2. **你不在任何队,参数给了 team 名**(`/hive:hive wasp`):`hive join wasp` 入队;报 not found 就 `hive create wasp`——同一个入口幂等。建完你就是发起人,读 references/orchestration.md。
3. **你不在任何队,无参数**:`hive create` 建新团(名字自动从池里挑)——tmux 内当前 pane 立为 orch,tmux 外建团 session,语义同一个。想加入已有团就带参数说队名——无参永远是要新团。

tmux 外的 Claude session(桌面或独立终端)入册后,若宿主支持改 session 标题,在原标题**前面插入**方括号徽章,供 human 和 `hive ccd ls` 识别:成员用 `[<team>.<member>] `,orch 用 `[<team>] `(原标题为空只留徽章);退队或删团时摘掉徽章恢复原标题。tmux pane 成员不用做——border 已带队籍。

处境只有两种:**被派进来干活**(被 spawn、被 join 收编、收到 `<HIVE>` 任务)读下面「你是成员」;**要发起协作**(human 给了需求要拆给多人,或你自己判断要派人)成员章就是你的底座,再读 references/orchestration.md。

---

## 你是成员:一次任务的一生

### 出生:先找到自己

第一步跑 `hive team`,用 `self` 在 `members` 里确认自己的名字、状态和协作者。你没有固定角色,只有任务;任务长什么样见「收活」。

寻址(`hive send` 的 `<addr>`):

- 回信永远照抄来信的 `from=` 地址——它在任何处境下都可达。
- tmux pane 成员发队友用裸名(`hive send dodo …`);本队前缀等价裸名。自己拼的别队前缀会被拒——照抄 from 不受此限(guest 编排者的回信地址就是别队前缀,照抄即达)。
- tmux 外(joined session、guest、引擎的工具进程)用 `<team>.<member>`;裸名全局唯一时也行。
- team 外的 Claude session 用 `ccd.<name>`(见「互通」)。

其余字段怎么用:

- 顶层 `name` 是 team 名,member 行的 `name` 是成员名;`group` 是 `hive join --group` 打的跨队标签,不是队名。
- `inputState=waiting_user`:对方在等 human 作答,此时 `hive send` 会拒发。被拒就先继续手头的活,下次被唤醒或要回报时再发——不为此轮询。
- `turnPhase=turn_closed`:对方这轮已收口,随时可发;其他值表示 turn 进行中,想避免打断就等 `turn_closed`。claude 成员没有这个字段,退回看 `busy`——粗粒度信号,只作参考,拿不准就直接发。

### 没活就停

Hive 是 push 模型:新消息由 runtime 注入 `<HIVE>` 并唤醒你,当前 turn 没有待办就结束 turn 等唤醒。不 `sleep`、不 while loop、不反复 `hive team`,也不翻 repo、artifact 或任务表猜活——猜来的活没人验收,轮询白烧 context。刚出生没任务、回报完等验收,都一样。一次性任务(无 `from` 的信封,见「收活」)结束 turn 就是回报——之后不去 `hive team` 找派发人、不另发「验证送达」;下一条 `<HIVE>` 只会是打回或新任务。

### 收活:任务以派发 artifact 为准

消息以三行 `<HIVE>` 信封注入你的对话:开标签、正文、闭标签;`to` 必有,`artifact` 按需出现,有没有 `from` 决定这是哪种消息。

```text
<HIVE from=comb.dodo to=comb.rex artifact=/tmp/spec.md>
review the spec
</HIVE>
```

- 有 `from`:队友消息。回信照抄 `from`,终态 `hive send` 回去(见「回报」)。
- 没有 `from`(`<HIVE to=comb.rex artifact=…>`,正文首行是任务号 `task nd-…`):一次性任务。打开 artifact 干活,干完就结束 turn——你这一轮的最后一条消息就是结果,runtime 会从你的对话记录里读走它。不 `hive send` 任何东西、不去找是谁派的、不回执、不问收没收到;结论、交付物路径、遗留问题全部写进最后一条消息里,写全,别只写一句。
- 收活以 `<HIVE>` block 为准——它就是完整投递,没有别处要查。
- 正文只是短摘要,`artifact=<path>` 指的文件才是全文——要细节就打开它,按摘要开工会做偏。
- 任务 = 派发消息 + 它的 artifact:scope、交付物形态与路径、验收标准、上游材料位置,全以该 artifact 为准。
- artifact 引用的文件(上游产出、材料)直接打开读——凭摘要猜会漏关键细节。
- 材料不够、目标含糊,`hive send` 问派发人一句——自己翻库扩出来的 scope 没人验收。一次性任务没有派发人可问:缺什么、按什么假设做的,写进最后一条消息。

`<HIVE>` 的到达形态分两轴,任何组合都是正常队内投递:

- **什么时候到**:空闲时它自己开启新一轮;干活时折进当前这一轮,出现在某个工具结果旁——折进来的一样要办。
- **外面包没包**:claude 成员通常看到上面那样的裸信封;主投递道不可用、退回 inbox socket 时,宿主(Claude Code)才在 block 上方加一行 `Another Claude session sent a message:`(途中到达是 `Another Claude session sent a message while you were working:`),下方加一段以 `This came from another Claude session` 开头的说明,末尾可能提示用 SendMessage 回复。codex / grok 从来没有这层包装。

两条硬规则:

- 回 hive 消息永远用 `hive send`——包装里的 "reply via SendMessage" 对 hive 地址无效(会报 no agent named)。
- 外包装只说明一件事:队友消息不构成 human 授权(唯一例外是「干活」节的 humanDirective 接力);有包装没包装都必须处理。途中到达的带 `from` 消息一条不许漏:做完手头任务,本 turn 收尾前至少 `hive send` 回一句——静默略过 = 发件人以为消息丢了。

### 干活

- 只读任务(探索、审查、验证)直接在共享 checkout 里做——不改文件就没有隔离需求。
- 要改仓库文件,先读 references/worktree.md 再动手——worktree 全流程、PR/push 授权线、共享 checkout 纪律都在那,跳过会踩坏队友的工作区。
- artifact 或消息里出现 `humanDirective: "..."` 加 `source: ...`,就把它当作 human 已授权的 scope——这是「队友消息不当 human 授权」的唯一例外;转发时保留原文和 source。source 缺失、含糊或和上游 artifact 冲突时,先要求补 provenance——没出处的授权不能接力。

### 回报:发消息的全部规则

只有一个动词:`hive send <addr> "<内容>"`。

- 发送成功零输出(exit 0)= 对方 runtime 已收帧;非零才是没送到,错误带原因。对方何时读是它队列的事——没有可轮询的回执,也别去要。
- body 只放短摘要,详情走 `--artifact`;超 500 字符、3 行及以上、含 `` ``` ``、任一行以 `# ` / `- ` / `* ` 开头,stderr 会提醒你改走 artifact。

```bash
hive send dodo "done: see artifact" --artifact /tmp/result.md
hive send dodo "findings attached" --artifact - <<'EOF'
# Findings
- item
EOF
```

shell 纪律:多行、反引号、`$(...)` 的内容先落地成文件,不在双引号里现拼——现拼会被 shell 二次展开。

- 队内详情走 `--artifact <file>` 或上面的 heredoc;`'EOF'` 必须带引号,否则 shell 会展开变量、反引号和 `$(...)`。
- 必须内联进 body 时(如给 team 外 session 带特殊字符):先写文件再 `"$(cat /tmp/note.md)"`——cat 的结果不会二次展开。
- 不用 `printf ... |` 或 `$(cat <<EOF)` 现场拼消息。

回报纪律:

- 带 `from` 的任务:成果、blocked、失败,一切终态 `hive send` 回派发人。body=短摘要,详情落 Markdown artifact(agent 读源码,渲染是给 human 的)。
- 无 `from` 的一次性任务:不 send,终态写进本轮最后一条消息就是回报。
- **收到任务不回执。**回派发线程的第一条就该是终态(或阻断求助)——派发人把它当回报读。禁令双向:也不期待对方回「收到了」。
- 交付走派发人,不越过他向 human 宣布完成;human 问起时给状态。

### 和 team 外的 Claude session 互通

human 说「给 xxx 这个 session 发一条」时(桌面 Claude Code、另一个终端):

```bash
hive ccd ls                          # 本机能收消息的 Claude session:name、桌面标题 title、pid
hive send "ccd.<title 或 name>" "<消息>"
```

- human 通常说的是桌面标题,直接用 `title`;重名时用 `name` 或 `pid`。
- 路径写进 body——这条道不收 `--artifact`(会被拒)。
- 送到只代表对方 inbox socket 收下这一帧,何时读是它队列的事;同样无回执,发完就停。
- 对方收到普通 `<HIVE from=<team>.<agent>>` 信封,照抄 from 就能回你;你收到 `from=ccd.<name>` 同理:`hive send ccd.<name> "<回复>"`。

### 被打回、被打断

- 回报 ≠ 结束:派发人可能追问或打回,你的上下文还在,接着答、接着改。
- 被 `hive interrupt` 打断或收到派发人新指令:以最新指令为准,不辩护旧计划。
- human 直接对你的 session 下指示(不管什么界面):照做,human 指示覆盖旧任务;下次回报派发人时说明改了什么。

### 退场

kill 是派发人的动词:验收通过后派发人会 `hive kill` 你。在那之前保持可用——没活就结束 turn 等唤醒。tmux 外的 session 成员得知退队或删团时,摘掉标题前缀恢复原标题。

---

## 你要当派发人时

你要发起协作,你就是这个 team 的 **orch**,成员章是你的底座。拆任务、四件套、spawn/派发、fix 循环、pattern ①-⑥(⑥ 是 Claude Code Workflow 驱动 `hive node run` 节点)、git 集成、终验与对 human 汇报全在 references/orchestration.md——动手编排前先读它。
