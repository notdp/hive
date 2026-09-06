# 编排:orch 手册

> 谁读:发起协作、拆任务的 orch 或 guest 编排者,读完 SKILL.md 成员章、动手编排前读;成员干活不用读。

## 就位

**orch** 负责拆任务、spawn/派发、收结论、集成终验、向 human 汇报。你不写业务代码——context 要留给验收。

- 还没有 team 就 `hive create`——建团者即 orch,tmux 内外一样:你以 `<team>.orch` 入册,成员直接寻址 orch。
- 唯一例外:你已是别团成员时不会再入册,本团以 guest 身份编排,回信地址仍是原队的 `<原team>.<你的成员名>`——成员照抄 from 就能回到你。
- 先 `hive team` 确认 `self` 再拆解。成员回信会注入并唤醒你,spawn/派发后照常结束 turn 等推送。

## 成员名就是任务标签

runtime 没有角色:spawn 时起的名字(`explore`、`impl-auth`、`review`)就是这个成员的全部身份,出现在消息地址和显示层。用语义化 kebab-case、≤4 词、看名知事。活着的成员集合就是 workflow 现状——环节推进用 spawn/kill 表达,不改名,名字一变地址和历史就对不上。

## task artifact 四件套

每个任务先写 `<workspace>/artifacts/tasks/<member>.md`(workspace 取 `hive team` 的 `runtimeWorkspace` 字段),必含:

1. **scope**:做什么、不做什么。
2. **交付物**:形态与产出路径(报告、commit 还是 PR)。
3. **验收标准**:你终验按什么判。
4. **材料**:上游产出的 artifact 路径。成员是全盲的,不知道 workflow 形状——它需要的一切材料都在这份 artifact 里给路径,别指望它自己发现。

## spawn + 派发

```bash
hive spawn explore --task <workspace>/artifacts/tasks/explore.md
hive spawn impl-auth --cli codex --task <workspace>/artifacts/tasks/impl-auth.md
```

- `--task` 把任务原子投递为成员的首条 `<HIVE>` 消息——成员不会空 inbox 出生。
- `--cli` 缺省跟你同 CLI(claude|codex|grok);tmux 外 spawn 没有 pane 可参照,缺省是 claude。要异构时必须显式传,见 pattern ①。
- model 不确定就别传,默认就是对的(不照抄状态栏,那不是 model id)。要传时:claude 用别名 `fable` / `opus` / `sonnet`——永远指向该档当前最新;典型分工 `fable` 做 verify/裁决,`opus` 做执行主力(集成验收不 spawn 成员,是你自己做,见 pattern ⑤)。codex/grok 传具体 id,按该 CLI 的 catalog 校验,打错带 did-you-mean 拒收。

成员完工会 `hive send` 回报你——自动锚回派发线程。读摘要,必要时读它的 artifact。

## 进度只来自回信

成员的进度信号只有三个:回报消息、notify 事件、`hive team` 的 runtime 字段。前两个是推送;`hive team` 只在收到消息后核对,不是轮询工具。派发后没有待办就结束 turn 等唤醒。

- 不用 `tmux capture-pane` 或任何读屏手段观察成员——屏幕有残屏和中间态,不是真相,读屏还烧你的 context。
- 已派发的任务不自己并行做一遍——你的产出没人验收,还烧掉终验要用的 context。

## 成员生命周期

- **验收前不 kill**。回报 ≠ 验收:不满意就 `hive send` 打回追问——活成员带全部上下文,杀掉重 spawn 会丢掉上下文。
- 验收通过、下游任务的 artifact 写好之后,`hive kill <member>`,再 spawn 下一环节。
- 唯一例外是 fix 循环(pattern ④):产出还会被打回的成员,留到下游 verify pass 再 kill。

## Pattern library

以下是建议模式,按任务自由组合;stage 划分、数量、顺序都是你的编排决定。

**① producer + 异构 reviewer**——改动需要独立审时,producer 和 reviewer 用不同家族的 CLI——**`--cli` 必须显式写**,忘了就是同构 review,白审:

```bash
hive spawn impl --task <workspace>/artifacts/tasks/impl.md
hive spawn review --cli codex --task <workspace>/artifacts/tasks/review.md
```

review 的 task artifact 里要求 verdict:`pass`/`fail` + evidence + required-changes。reviewer 独立审计,不照抄 producer 叙事;关键结论从 diff、日志、命令输出自己核。

**② solo 快任务**——一个成员闭环一件小事。spawn → 回报 → 验收 → kill。

**③ explore → impl 接力**:

```text
spawn explore ──> 回报(摘要+findings artifact) ──> 验收 ──> kill explore
                          impl 的 task artifact 引用 findings 路径
                                  ──> spawn impl
```

接力棒是 artifact 文件,不是活成员。你只过目摘要,不搬运正文。

**④ fix 循环**——impl 回报后**不 kill**,spawn verify(task 带验收标准、branch、impl 报告路径):

- verify fail → `hive send` 把 required-changes 打回给 impl——同成员带上下文修得快;verify 也留着,复验时记得上次挂在哪。
- verify pass → kill impl + verify。
- 默认 5 轮上限(task artifact 可覆写),到限升级 human。

**⑤ 集成验收**——所有任务 DONE 后,你自己拉集成分支、跑测试、核验收标准,过了才向 human 汇报。终验不外包。

**⑥ hive 节点进 Claude Code Workflow**——你在用 Claude Code 的 Workflow 工具编排时,可以让某个节点是活的 hive 成员(可见 pane,human 可介入),同时保留 Workflow 自己的进度树和 journal。循环、fan-out、barrier 这类控制流由 Workflow 脚本表达,hive 只提供节点。

开工:在编排的 Claude session 里 `hive create <run>`——建团者即 orch,你以 `<run>.orch` 入册,团窗口首格是你的只读镜像;human `hive attach <run>` 看全场。session=team=run 名。

节点就是一条阻塞命令:`hive workflow run --team <run> --name <member> --cli codex|grok [--model]`,task 从 stdin 进,结果以一行 JSON 从 stdout 出。节点像 Workflow 自己的子代理一样工作:成员不被要求回信,也不跑任何返回命令。它收到的是一封没有 `from` 的信封——`<HIVE to=<run>.<member> artifact=<workspace>/artifacts/tasks/<member>-<nd-…>.md>`,正文首行是任务号 `task nd-…`——任务就是这一轮,runner 等的是引擎自己报的这一轮结束(codex `turn/completed`,grok `session/prompt` 的响应),拿到的 body 是成员这一轮**最后说的那段话**。成员中途停下来提问,这一轮就结束了,那个问题就是它的返回值。task 里仍要写明最后那段话里该有什么(commit sha、报告路径、verdict),而不是「完成后回报」。节点只能是 codex 或 grok:claude 节点用 Claude Code 自己的子代理(`agent(...)` 不带 `agentType`),hive 不提供。

JSON 字段:`status`、`name`、`pane`、`reused`、`dispatchId`(`nd-` 开头);这一轮结束了就有 `body`(最后那段话,可能为空串),`completed` 以外都有 `reason`。`status` 取值:

- `completed`:引擎正常收口(codex `completed`,grok `end_turn`),body 就是成员最后说的。
- `interrupted`:这一轮被打断(codex `interrupted`,grok `cancelled`),body 是打断前说到的。
- `failed`:引擎以错误收口(codex `failed`,grok 出错响应、`max_tokens`、`refusal`…),`reason` 带引擎原话。
- `no_result`:这一轮没在跑、也没人拿着它的结果——hived 在派发后重启过,或 120 次轮询都没应答;任务可能做了也可能没做,由脚本决定重派还是看 pane。
- `member_gone`:等待期间成员死了。
- `member_busy`:没派发——上一跑还 pending 且成员活着、名字被别的 runner 锁着、或成员 600 次轮询还在一轮里没空(runner 不往进行中的一轮里塞任务)。

非 `completed` 一样是节点的返回值,由脚本决定重派、改任务还是升级 human,代理不重试、不解读。

派发失败分两种,runner 按 hived 的应答区分:hived 明确拒了(`ok:false`:传输拒收、成员不存在、send gate)或请求根本没送到,是"确定没派出去"——重试 3 次,最终拒收就撤掉 pending 记录、回收本次 spawn 的成员、exit 1;请求发出去了但应答没回来(socket 读超时、连接断、空应答),是"不知道派没派出去"——任务可能已经注入,**绝不重发**,记录留在 pending(`seq` 为 null),照常去 hived 读这一轮的结果:hived 无论应答有没有送回来都按任务号拿着这一轮,所以丢应答只丢一个 seq,结局和正常派发一样。

exit code 的语义只有一条:exit 1 = 任务没派发出去(team 不对、指定了 claude、spawn/ready 失败),可以直接重跑;`member_busy` 也是没派发,但以 exit 0 + JSON 报,由脚本决定等还是换人;派发出去的任务一定以 exit 0 + 一行 JSON 收场,turn 本身没有超时,由脚本决定等多久。每次跑在 `<workspace>/run/workflow/<member>.json` 留记录,派发前先写 pending。两条 v1 限制:runner 被 Ctrl-C 杀掉时记录留在 pending,直到 `hive kill` 该成员才清;拿到终态 verdict 就释放名字,但成员可能还在干——`no_result` 之后别立刻同名重派,先看 pane 或 kill 掉重 spawn。

hive 插件分发的 `hive-node` 代理 agent 就只做这一件事——把这条命令挂后台跑、循环等它的 exit 文件(单次 Bash 有十分钟上限,所以是同一条等待命令反复调用,不是"待会再看")、完成后把 JSON 原样交回 workflow。写法:prompt 第一行是这条命令,其余是 task:

```js
const result = await agent(`hive workflow run --team ${run} --name impl-auth --cli codex

实现 auth 模块;交付 commit;最后一段话写 commit sha 和改动说明文件的路径。`, { agentType: 'hive-node', label: '⬡ impl-auth 「codex」', schema: ... })
```

代理定义里已固定 `model: haiku`,不用在调用处写。Workflow 面板的 Model 列显示的是**代理**的模型,成员真身的 CLI/模型没有任何接口能注入该列——唯一的显示杠杆是 label 自由文本。约定:`⬡ <name> 「<cli>」`,显式指定了成员模型时写进容器,如 `⬡ impl-auth 「codex · gpt-5.4」`。

成员生命周期归你:workflow 结束后成员还活着,同名节点再跑一次会复用活成员(带上下文);不要了就 `hive kill <name>`。跑完 `hive delete <run> --down`(kill 全部成员 + 删 team + 杀 session),或留团供追问、拆时再清。agent 定义是 session 启动时注册的——本 session 中途才装上插件的话,把同样的四步(mktemp 落任务文件、后台起命令、循环等 exit 文件、原样返回 JSON)直接内联进 prompt 也一样跑。

## git / 集成纪律

单任务改动直接按 references/worktree.md 走。多个写码任务并行时,先建集成分支再派发:

```bash
git branch <team>-integration <base>
git push -u origin <team>-integration
hive worktree set-base <team>-integration
```

**必须先 set 再 spawn 写码成员**——成员的 `hive worktree start` 才会 base 到集成分支。补救:漏 push 时成员开 PR 会报 base 不存在,它会报给你,你补 push。

merge 串行一次一条,只由你做,且在该任务验收通过、human 批准后:

```bash
gh pr merge <PR号> --match-head-commit <验过的head> --squash
```

- 必须带 PR 号,必须带 `--match-head-commit`——避免 pass 后又 push 的 commit 被误合。
- 每合一条,通知 in-flight 写码成员 rebase;它们重跑 start 会拿到 `needs-rebase`。
- 冲突在 PR / 集成点处理——worktree 只隔离工作区,不消除冲突。

首个 sub-PR 合入后可以开 main PR:集成分支 -> main。human review / merge main PR 是最终交付。

## 对 human

- 只给已收敛结论、单个阻断问题、建议下一步——human 的注意力留给拍板。拍板用所在 CLI 的阻塞提问工具(claude 是 `AskUserQuestion`)。
- 成员越过你直接向 human 交付时,回它「终态发我」——交付线走你,验收才有着落。
- human 直接对某个成员改了方向:以 human 为准,更新你手里的验收标准;成员回报时会说明。
- stage 汇报和最终交付要有自包含 HTML,Markdown 源同目录,发 human 时给 HTML 绝对路径;agent 间 artifact 一律 Markdown——human 看渲染,agent 读源码。
- 全部完成且 human 签字后,才 kill 剩余成员;整团收摊用 `hive delete`。

## 窗口相关

- 布局拖乱了跑 `hive layout auto`。
- PR 号钉窗口状态栏:`hive pr set <PR号>` / `hive pr clear`。
