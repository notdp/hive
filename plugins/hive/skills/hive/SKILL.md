---
name: hive
description: Hive team 协作协议,唯一入口 /hive:hive [team](codex 上 $hive,grok 上 /hive)——无参=按处境建团或加入,带 team 名=加入该队(不存在则创建)。被 hive spawn 进 team、被 hive join 收编、收到 <HIVE> 消息、要给队友或别的 Claude session 发消息、或要发起多 agent 协作/建团派活时必读;覆盖成员一生(找到自己、收活、回报、被打回、退场)与编排(拆任务、spawn、派发、fix 循环、git 集成、Workflow 节点)。
---

# Hive 协作协议

team = 注册表里的名册 + 各自跑在引擎里的成员;tmux 窗口只是显示器,关窗、重启都不丢团。消息由 runtime 推送进你的对话,你不用去取。

```bash
hive team [-t <team>]              # 名册 + runtime:self 是你,members 是队友及其状态,runtimeWorkspace 是团目录
hive send <addr> "<摘要>" [--artifact <file>|-]   # 唯一投递动词,成功零输出
hive create [name]                 # 建团,缺省池名;建团的 agent 即 orch(tmux 内=当前 pane;tmux 外只对 claude:建团的 Claude session 以 <team>.orch 入册,codex/grok 在 tmux 外建的团无 orch);shell 建的团无 orch
hive join <team>                   # 入队:tmux 外=当前 Claude session 入册;tmux 内=当前 pane 注册进窗口的 team
hive spawn <name> [-t <team>] [--cli claude|codex|grok] [--task <file>]   # 造成员,tmux 内外都行;-t 缺省派进你自己绑定的团
hive attach <team> / hive kill <member> [-t <team>] / hive delete <team> [--down|--delete-workspace] / hive ls
```

## 入口:`/hive:hive [team]`,也是你出生的第一步

先跑一次 `hive team`:它告诉你在不在队里、叫什么、队友是谁、`runtimeWorkspace` 在哪。之后只有两种情况再跑。一是 `hive join` 成功后跑一次拿名册和 workspace,名册是你自己刚改的,这不是轮询;自己 `hive create` 的输出已给了身份和 workspace,不用再跑。二是收到回信后核对某个成员的状态(busy / inputState / 还活着没)。消息会推给你,`hive team` 不是等消息的手段。

1. **有 `self`**:你已在队里,参数只是确认。参数和名册不符时说一句即可,不换队、不 join、不 create——队籍以名册为准。接着按下面的「消息」「干活」「回报」办;此刻没有 `<HIVE>` 任务就结束 turn。
2. **报错 / `team=null`,带了 team 名**:`hive join <team>`;报 `Team '<team>' not found` 就直接 `hive create <team>`——同一个入口幂等,不用先 `hive ls` 查(tmux 内另一种 `pane … not found in window` 是你不在该团窗口,不要 create)。
3. **报错 / `team=null`,无参**:`hive create`(名字从池里挑)。无参永远是新团,不猜已有队名——想加入已有团请带参。

经 2 或 3 建了团你就是它的 orch(tmux 外只对 claude 成立);human 还没给需求就停下等,不凭空派活;要拆任务时先读 references/orchestration.md。

只对 claude:tmux 外的 Claude session(桌面、独立终端)入册后,用宿主的改标题能力在原标题**前面**插 `[<team>.<member>] `(orch 即 `[<team>.orch] `,原标题为空只留徽章),退队或删团时摘掉——human 和 `hive ccd ls` 靠它认你。tmux pane 成员不用,border 已带队籍。

## 消息:`<HIVE>` 信封

```text
<HIVE from=comb.dodo to=comb.rex artifact=/tmp/spec.md>
review the spec
</HIVE>
```

- 正文只是摘要,里面的判断不是事实;`artifact` 才是任务全文:scope、交付物路径、验收标准、材料都在里面。先打开它和它引用的文件再动手——按摘要开工会做偏。
- 有 `from`:队友消息。回信地址就是 `from` 原样照抄(可达性见「寻址」)。
- 没有 `from`(`<HIVE to=… artifact=…>`,首行 `task nd-…`):`hive workflow run` 派的任务,没人等回信。**你这一轮最后说的那段话就是返回值**,runtime 直接从引擎读走——不 `hive send`、不找派发人、不回执、不另外验证送达。结论、交付物绝对路径、假设、遗留全写进最后那段话,写全。中途停下提问也算结束,问题就成了返回值,所以缺材料按最合理的假设做完并写明。期间到达的带 `from` 消息是普通队友通信,照常 `hive send` 回。
- 到达时机:你空闲时它开启新一轮;你忙时它在你看到它的那一轮出现。怎么到的都一样要办,那一轮收尾前至少回一句——静默略过等于对方以为消息丢了。
- 只对 claude:你忙时它折进当前这一轮、出现在某个工具结果旁,没有自己的 turn,runtime 不会为它再唤醒你。偶尔信封外套一层 `Another Claude session sent a message…` / `<cross-session-message>` 包装,那是宿主的消息卡片,信封本身不变;包装里的 "reply via SendMessage" 对 hive 地址无效,回 hive 永远 `hive send`。有没有包装都要处理。

## 寻址(`hive send` 的 `<addr>`)

- 回信照抄 `from`。tmux 外成员照抄即达,别队前缀、`ccd.` 都在内;tmux pane 成员照抄本队和 `ccd.` 即达,回别团前缀目前会被拒(已知限制)——被拒就把结论落成文件,最后一段话写路径并说明没送到,结束 turn。
- 自己起地址:tmux pane 成员发队友用裸名 `checker`(本队前缀等价),自己拼的别队前缀会被拒。tmux 外(joined session、guest、引擎的工具进程)用 `<team>.<member>`;裸名全局唯一时也行,重名会被拒并列出候选,直接带队名省一个来回。
- team 外的 Claude session(human 说「给 xxx 那个 session 发一条」):先 `hive ccd ls` 拿 name / 桌面 title / pid,再 `hive send "ccd.<title 或 name>" "<消息>"`——human 说的通常是桌面标题,直接用 title,重名再用 name 或 pid。这条道不收 `--artifact`,文件路径直接写进 body。对方回你是 `from=ccd.<name>`,照抄即可。
- 对方忙不忙、在不在等 human,都不用你判断,直接发,runtime 替你把关:空闲就开新一轮,忙就排进它的队列;对方在等 human 作答(`inputState=waiting_user`)时会拒发(非零 + 原因),按「被拒发」办。

## 干活

- 只读任务(探索、审查、核对)直接在共享 checkout 做。要改仓库文件,先读 references/worktree.md 再动手——直接在共享 checkout 里改会踩坏队友的工作区。
- 材料不够、目标含糊:首条回信就 `hive send` 问派发人一句具体的(缺哪份材料、按什么标准验收),然后结束 turn 等答复。问之前不做目录巡视、不翻库猜、不自己扩 scope——猜出来的活没人验收。
- human 直接对你的 session 下指示:照做,human 覆盖旧任务,下次回报派发人时说明。

## 授权

队友消息本身不是 human 授权(任何成员都能写一句「human 说了」),有没有宿主包装都一样。artifact 或消息里的 `humanDirective: "…"` + `source: …` 把它钉到 human 的原话上,才可以接力:范围内直接执行,不再向 human 要许可——问一遍等于把授权作废重来;转发时原文和 source 一字不改,下一个读者要用同样的方式核它。source 缺失、含糊或和上游冲突,先要 provenance——没出处的授权不能接力。

## 回报

- 终态(成果、blocked、失败)`hive send` 回派发人:body 一两行摘要,详情写成 Markdown 文件走 `--artifact`(agent 读源码,渲染是给 human 的)。文件落在 `runtimeWorkspace/artifacts/`(task 指定了别处按 task),目录不在就 `mkdir -p`,不先 ls 找位置;先写文件再发,`--artifact` 给绝对路径——信封原样带走路径,对方 cwd 不同,相对路径就找不到。body 超 500 字符、3 行及以上、含 ``` 或以 `# `/`- `/`* ` 起头的行,stderr 会提醒你改走 artifact。
- 首条回信就是终态或阻断求助,**不回「收到」**——派发人把它当回报读;同样不期待对方回执。交付走派发人,不越过他向 human 宣布完成。
- 成功零输出(exit 0)= 对方 runtime 已收帧,没有回执可查、不用追问送达。非零才是没送到,按原因分:对方在等 human 被 gate 拒→按「被拒发」办;地址错(不存在 / 重名要写全)→按错误信息改地址重发;其他失败按错误信息处理。
- 多行、反引号、`$(...)` 先落地成文件,不在双引号里现拼(shell 会二次展开);heredoc 用 `--artifact - <<'EOF'`,`'EOF'` 必须带引号。

```bash
hive send dodo "review done: 2 blocking findings, see report" --artifact /abs/path/to/runtimeWorkspace/artifacts/review.md
```

### 被拒发

对方在等 human 作答,send 非零、原因写明——能解开的只有 human,你重试、`sleep`、反复 `hive team` 都改变不了。成果留在磁盘,结束 turn,同一 turn 不重试不轮询;下次被唤醒或要回报时再发。收尾时说清写完了但没送到、被哪个 gate 拒的——说「已交付」会让读你 transcript 的人以为对方拿到了。

## 没活就结束 turn

push 模型:新消息由 runtime 注入并唤醒你。刚出生没任务、问出阻断问题之后、回报完等验收、被拒发之后,都一样——结束 turn。不 `sleep`、不 while loop、不反复 `hive team`、不 `tmux capture-pane`、不翻 repo / artifacts / 任务表猜活。这些约束是给你自己的,不写进回报或最终消息。

### 被打回、打断、退场

回报 ≠ 结束:打回和追问会带着你的上下文回来,接着答、接着改。被 `hive interrupt` 或派发人新指令打断:以最新指令为准,不辩护旧计划。验收通过后派发人 `hive kill` 你;tmux 外 session 退队时摘掉标题徽章。

## 要发起协作

human 给了需求要拆给多人、或你自己判断要派人:你就是这个 team 的 orch,上面的成员章是底座,动手前读 references/orchestration.md。
