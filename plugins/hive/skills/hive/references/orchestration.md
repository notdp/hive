# 编排:orch 手册

> 谁读:发起协作、拆任务的 orch,或已是别团成员、以 guest 身份编排的人。读完 SKILL.md、动手编排前读;成员干活不用读。

## 就位

orch 拆任务、写 task artifact、spawn/派发、收回信、集成终验、向 human 汇报。你不写业务代码——context 要留给验收。

- 建团者即 orch,tmux 内外一样:你以 `<team>.orch` 入册,成员直接寻址 `orch`。唯一例外是 tmux 外、已是别团成员的 session 再 `hive create <新团>`:你不入册,以 guest 编排新团(tmux pane 成员做不到——已绑定的 pane 跑 `hive create <别名>` 只会幂等返回本团)。guest 有两个地址:**回信地址**仍是你原队的 `<原team>.<你>`;**派发作用域**不会自动切到新团——每个动词缺省作用在你自己绑定的团,`hive create <新团>` 不改这个绑定,所以对新团的每条 `spawn` / `kill` / `team` 都显式 `-t <新团>`,看新团名册用 `hive team -t <新团>`(裸 `hive team` 回的仍是原队):

```bash
hive spawn lint-docs -t maple --task <新团workspace>/artifacts/tasks/lint-docs.md
```

- workspace 从哪来:入口时已在队里→入口那次 `hive team` 的 `runtimeWorkspace`;自己 `hive create` 的→create 输出里的 workspace;`hive join` 的→join 后再跑一次 `hive team`;guest→`hive team -t <新团>`。缺省都是 `$HIVE_HOME/teams/<team>/`。
- 拆解从 human 的需求开始;还没有需求就结束 turn 等,不凭空安排工作。spawn/派发后同样结束 turn,回信会注入并唤醒你。

## 成员名就是任务标签

runtime 没有角色:spawn 时起的名字(`explore`、`impl-auth`、`review`)就是成员的全部身份,出现在地址和显示层。语义化 kebab-case、≤4 词、看名知事。活着的成员集合就是 workflow 现状,环节推进用 spawn/kill 表达,不改名——名字一变,地址和历史就对不上。

## task artifact 四件套

每个任务先写 `<workspace>/artifacts/tasks/<member>.md`,四项都要有:

1. **scope**:做什么、不做什么。
2. **交付物**:形态与产出路径(报告、commit 还是 PR)。
3. **验收标准**:你终验按什么判,写成能执行的检查。
4. **材料**:上游产出、输入数据的绝对路径。成员是全盲的,不知道 workflow 长什么样——它需要的一切都在这份文件里给路径,别指望它自己发现。

## spawn + 派发

```bash
hive spawn explore --task <workspace>/artifacts/tasks/explore.md
hive spawn review --cli codex --task <workspace>/artifacts/tasks/review.md
```

- `--task` 把任务原子投递为成员的首条 `<HIVE>`,成员不会空 inbox 出生;文件要在 spawn 前写好。
- `--cli` 缺省跟你同 CLI(claude|codex|grok);tmux 外 spawn 没有 pane 可参照,缺省 claude。要异构必须显式传。
- model 不确定就别传,默认就是对的(状态栏显示的不是 model id,别照抄)。要传:claude 用别名 `fable`/`opus`/`sonnet`——永远指向该档当前最新,典型分工 `fable` verify/裁决、`opus` 执行主力;codex/grok 传具体 id,按该 CLI 的 catalog 校验,打错带 did-you-mean 拒收。

成员完工会 `hive send` 回报你。读摘要,必要时读它的 artifact。

## 进度只来自回信

成员进度只有三个来源:回报消息、notify 事件、`hive team` 的 runtime 字段。前两个是推送;`hive team` 只在收到消息后核对,不是轮询工具。`spawn` exit 0 且输出 `dispatched: true` 就是派发落地的凭据——不跑 `hive team` 复核名册,名册不是回执。不 `tmux capture-pane` 或任何读屏手段观察成员——残屏和中间态不是真相,还烧你的 context。已派发的任务不自己再做一遍:你的产出没人验收,还烧掉终验要用的 context。

## 成员生命周期

- 验收前不 kill:回报 ≠ 验收,不满意 `hive send` 打回追问——活成员带全部上下文,杀掉重 spawn 丢的就是它。
- 验收通过、下游任务的 artifact 写好后 `hive kill <member>`,再 spawn 下一环节。
- 唯一例外是 fix 循环(④):产出还会被打回的成员留到 verify pass 再 kill。

## Pattern library

建议模式,按任务自由组合;stage 划分、数量、顺序都是你的编排决定。

**① producer + 异构 reviewer**:改动需要独立审时,reviewer 用不同家族的 CLI,`--cli` 必须显式写——忘了就是同构 review,白审。review 的 task 要求 verdict `pass`/`fail` + evidence + required-changes,并给 reviewer 和 producer 同样的原始材料路径——只让它读 producer 的报告,它就只能复述;关键结论从 diff、日志、材料自己核。

**② solo 快任务**:一个成员闭环一件小事。spawn → 回报 → 验收 → kill。

**③ explore → impl 接力**:explore 回报(摘要 + findings artifact)→ 验收 → kill explore → impl 的 task 引用 findings 路径 → spawn impl。接力棒是 artifact 文件,不是活成员;你只过目摘要,不搬运正文。

**④ fix 循环**:impl 回报后不 kill,spawn verify(task 带验收标准、branch、impl 报告路径)。fail → required-changes 打回 impl,verify 也留着复验;pass → kill 两者。默认 5 轮上限(task 可覆写),到限升级 human。

**⑤ 集成验收**:所有任务 DONE 后你自己拉集成分支、跑测试、核验收标准,过了才向 human 汇报。终验不外包。

**⑥ hive 节点进 Claude Code Workflow**:用 Workflow 工具编排时,节点可以是活的 hive 成员(可见 pane,human 可介入),Workflow 保留自己的进度树和 journal;循环、fan-out、barrier 由 Workflow 脚本表达,hive 只提供节点。

开工:编排的 Claude session 里 `hive create <run>`,你以 `<run>.orch` 入册,团窗口首格是你的只读镜像;session=team=run 名;human `hive attach <run>` 看全场。

节点是一条阻塞命令:`hive workflow run --team <run> --name <member> --cli codex|grok [--model]`,task 从 stdin 进,结果一行 JSON 从 stdout 出。成员收到的是没有 `from` 的信封(`<HIVE to=<run>.<member> artifact=<workspace>/artifacts/tasks/<member>-<nd-…>.md>`,首行 `task nd-…`),不被要求回信、不跑任何返回命令;runner 等引擎自己报的这一轮结束(codex `turn/completed`,grok `session/prompt` 的响应),`body` 就是成员这一轮最后说的那段话,中途提问就以那个问题结束。所以 task 里写明最后那段话该有什么(commit sha、报告路径、verdict),不写「完成后回报」。节点只能 codex 或 grok;claude 节点用 Workflow 自己的子代理(`agent(...)` 不带 `agentType`),hive 不提供。

JSON 字段:`status`、`name`、`pane`、`reused`、`dispatchId`(`nd-` 开头);这一轮结束了就有 `body`(可能为空串),`completed` 以外都有 `reason`。`status` 取值:

- `completed`:引擎正常收口(codex `completed`,grok `end_turn`)。
- `interrupted`:被打断(codex `interrupted`,grok `cancelled`),body 是打断前说到的。
- `failed`:引擎以错误收口(codex `failed`,grok 出错响应、`max_tokens`、`refusal`…),`reason` 带引擎原话。
- `no_result`:这一轮没在跑,也没人拿着它的结果(例如 hived 在派发后重启过);任务可能做了也可能没做,重派前先核副作用。
- `unknown`:120 次结果轮询都没应答,等待者退出但执行未决;记录仍占用成员,同名重跑先按旧 dispatchId 问 hived 裁决。
- `member_gone`:等待期间成员死了。
- `member_busy`:没派发——上一跑 pending/unknown 仍未决且成员活着、名字被别的 runner 锁着、或成员 600 次轮询还在一轮里(runner 不往进行中的一轮里塞任务)。

非 `completed` 一样是节点的返回值,由脚本决定重派、改任务还是升级 human;代理不重试、不解读。exit 1 = 任务没派发(team 不对、指定了 claude、spawn/ready 失败、hived 明确拒收三次),可直接重跑;`member_busy` 也是没派发,但 exit 0 + JSON;派出去的任务一定 exit 0 + 一行 JSON,turn 本身没有超时。派发请求发出去了但应答没回来时 runner 绝不重发——任务可能已注入——记录留在 pending,照常去 hived 读结果;同名重跑先查旧 dispatchId:已结束的先存结果,turn 明确关闭的视为陈旧,仍在跑或问不到的返回 `member_busy`。记录在 `<workspace>/run/workflow/<member>.json`,`hive kill` 成员会删掉它;`no_result` 不说明任务有没有产生副作用。

插件的 `hive-node` 代理只做一件事:把命令挂后台跑、循环等它的 exit 文件(单次 Bash 有十分钟上限,所以是同一条等待命令反复调用,不是"待会再看")、完成后把 JSON 原样交回。写法:prompt 第一行是命令,其余是 task:

```js
const result = await agent(`hive workflow run --team ${run} --name impl-auth --cli codex

实现 auth 模块;交付 commit;最后一段话写 commit sha 和改动说明文件的路径。`, { agentType: 'hive-node', label: '⬡ impl-auth 「codex」', schema: ... })
```

代理定义里已固定 `model: haiku`。Workflow 面板 Model 列显示的是代理的模型,成员真身的 CLI/模型只能靠 label 体现:约定 `⬡ <name> 「<cli>」`,显式指定了模型时写进容器,如 `⬡ impl-auth 「codex · gpt-5.4」`。agent 定义是 session 启动时注册的,本 session 中途才装插件的话,把同样四步(mktemp 落任务文件、后台起命令、循环等 exit 文件、原样返回 JSON)内联进 prompt 也一样跑。

成员生命周期归你:workflow 结束后成员还活着,同名节点再跑会复用活成员(带上下文);不要了 `hive kill <name>`。跑完 `hive delete <run> --down`(kill 全部成员 + 删 team + 杀 session),或留团供追问、拆时再清。

## git / 集成纪律

单任务改动按 references/worktree.md 走。多个写码任务并行时,先建集成分支,**再 spawn 写码成员**——成员的 `hive worktree start` 才会 base 到它:

```bash
git branch <team>-integration <base>
git push -u origin <team>-integration
hive worktree set-base <team>-integration   # 写在团窗口上:在团窗口的 pane 里跑;tmux 外的 orch 让一个成员代跑
```

漏 push 时成员开 PR 会报 base 不存在,它会报给你,你补 push。

merge 串行一次一条,只由你做,且在该任务验收通过、human 批准后:`gh pr merge <PR号> --match-head-commit <验过的head> --squash`——必须带 PR 号和 head,免得 pass 后又 push 的 commit 被误合。每合一条,通知 in-flight 写码成员 rebase(它们重跑 start 会拿到 `needs-rebase`)。冲突在 PR / 集成点处理,worktree 只隔离工作区,不消除冲突。首个 sub-PR 合入后可开 main PR(集成分支 → main),human review / merge 它才是最终交付。

## 对 human

- 只给已收敛结论、单个阻断问题、建议下一步——human 的注意力留给拍板。拍板用所在 CLI 的阻塞提问工具(claude 是 `AskUserQuestion`)。
- 给 human 的可执行命令(`hive attach <team>`、`hive view` 等)单独放 ```bash 围栏块,一条命令一块,不带 `$` 和输出——Claude Code Desktop 只给 shell 块加 Run 按钮,行内反引号只是文字。
- 成员越过你直接向 human 交付:回它「终态发我」,交付线走你,验收才有着落。human 直接对某个成员改了方向:以 human 为准,更新你手里的验收标准。
- stage 汇报和最终交付要有自包含 HTML,Markdown 源同目录,发 human 时给 HTML 绝对路径;agent 间 artifact 一律 Markdown。
- 全部完成且 human 签字后才 kill 剩余成员;整团收摊用 `hive delete`。

## 窗口

布局拖乱了 `hive layout auto`;PR 号钉状态栏 `hive pr set <PR号>` / `hive pr clear`。
