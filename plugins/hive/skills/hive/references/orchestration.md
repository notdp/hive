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
- `--cli` 缺省跟你同 CLI(claude|codex|grok);headless spawn 没有 pane 可参照,缺省是 claude。要异构时必须显式传,见 pattern ①。
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

**⑥ flow 脚本(机械流程)**——循环、fan-out、barrier 这类确定性控制流不用手工编排:写一个 JavaScript 脚本交给 `hive flow run`,每个 `agent()` 都是真实成员,human 全程可见可介入。`agent()` 走的是 pane spawn,所以团必须有 tmux 窗口(headless 团用不了);跑脚本的人不必在 tmux 里。

```js
// workflow.js
export const meta = { name: 'auth-work', description: '探索认证模块并实现' }

phase('Explore')
const f = await agent('探索认证模块;产出写 <workspace>/artifacts/f.md;完成后回报', { name: 'explore' })
phase('Build')
const [a, b] = await parallel([
  () => agent(`实现 auth,材料见 ${f.artifact};交付 commit`, { name: 'impl-auth' }),
  () => agent('实现 db 层;交付 commit', { name: 'impl-db', cli: 'codex' }),
])
phase('Verify')
const v = await agent(`验证 ${a.artifact} ${b.artifact};给 pass/fail verdict`, {
  name: 'verify', cli: 'codex',
  schema: { type: 'object', required: ['verdict'], properties: { verdict: { type: 'string', enum: ['pass', 'fail'] }, reasons: { type: 'array', items: { type: 'string' } } } },
})
if (v.verdict === 'fail') {
  await ask('impl-auth', `打回:按 ${v.reasons.join('; ')} 修`)   // 同成员带上下文修
}
await kill('verify')
return { verdict: v.verdict }
```

- 脚本必须以纯字面量 `export const meta = { name, description }` 开头;脚本体跑在 async 上下文里,顶层 `await` 和 `return` 都可用,`return` 的值就是 run 的最终输出(stdout 最后一行;进度行走 stderr)。
- 跑法:后台 shell 跑 `hive flow run workflow.js`,结束当前 turn 等完成通知,完成后读输出;期间来消息照常处理。
- API 全貌(不需要读源码):
  - `agent(prompt, { name, cli, model, schema })`——spawn+原子投递+阻塞等回报。prompt 就是 task artifact,写全四件套;`name` 必填,成员之后一律按名字引用。不带 `schema` 返回 `{ body, artifact, msgId }`;带 `schema` 时回信 body 必须是符合它的纯 JSON,返回校验过的对象(不合格自动打回重问两次,仍不合格才 throw)。
  - `ask(name, prompt, { schema? })`——对活成员追问/打回,阻塞等回答,返回同上。
  - `kill(name)`——验收后退场。
  - `parallel(thunks) -> list`——并发跑,按调用顺序返回;失败的分支落为 `null`(不中断其他分支),用 `.filter(Boolean)` 收敛。
  - `pipeline(items, ...stages) -> list`——逐 item 流水线,stage 间无 barrier;stage 回调拿 `(prev, item, i)`,某 stage 抛错该 item 落为 `null` 并跳过后续 stage。
  - `phase(title)`——标一个阶段:之后 spawn 的成员都挂在这个阶段下,`hive flow board` 按它分组显示串并行;`log(msg)` 打进度行。
- 确定性契约:`Date.now()`/`Math.random()`/无参 `new Date()` 在脚本里会 throw——因为每次 run 的 op 都记进 journal,`hive flow run workflow.js --resume <run-id>`(run id 在开跑第一行打出)会重放未变化的前缀:还活着的成员直接复用不重生,改了 prompt 就变成对活成员的追加派发,挂掉的成员才重 spawn。
- 动态判断仍然手工编排;脚本只接机械流程。

**⑥b hive 节点进 Claude Code Workflow**——你在用 Claude Code 的 Workflow 工具编排时,可以让某个节点是活的 hive 成员(可见 pane,human 可介入),同时保留 Workflow 自己的进度树和 journal。节点就是一条阻塞命令:`hive flow node run --team <run> --name <member> [--cli] [--model] [--phase <阶段>]`,task 从 stdin 进,回信以一行 JSON 从 stdout 出。hive 插件分发的 `hive-node` 代理 agent 就只做这一件事——把这条命令挂后台跑、循环等它的 exit 文件(单次 Bash 有十分钟上限,所以是同一条等待命令反复调用,不是"待会再看")、完成后把 JSON 原样交回 workflow。写法:prompt 第一行是这条命令,其余是 task:

```js
const reply = await agent(`hive flow node run --team ${run} --name impl-auth --cli codex --phase Build

实现 auth 模块;交付 commit;完成后回报。`, { agentType: 'hive-node', label: '⬡ impl-auth 「codex」', schema: ... })
```

`--phase` 写 workflow 自己的 phase 标题,看板就按它分组。代理定义里已固定 `model: haiku`,不用在调用处写。Workflow 面板的 Model 列显示的是**代理**的模型,成员真身的 CLI/模型没有任何接口能注入该列——唯一的显示杠杆是 label 自由文本。约定:`⬡ <name> 「<cli>」`,显式指定了成员模型时写进容器,如 `⬡ impl-auth 「codex · gpt-5.4」`。

**rig 约定**(workflow 专属 team):session=team=run 名。开工 `hive flow rig <run> [--orch <你的 session id>]`——一条命令建好 tmux session、同名 team、底部全宽 `hive flow board` 看板条,`--orch` 再挂一格 `hive view` 只读镜像;human `hive attach <run>` 看全场。看板的串并行分组直接来自节点的 `--phase`,不用另写任何文件。跑完 `hive flow rig <run> --down`(kill 全部成员 + 删 team + 杀 session),或留团供追问、拆时再清。

成员生命周期归你:workflow 结束后成员还活着,同名节点再跑一次会复用活成员(带上下文);不要了就 `hive kill <name>` 或 `--down`。agent 定义是 session 启动时注册的——本 session 中途才装上插件的话,把同样的三步(后台起命令、循环等 exit 文件、原样返回 JSON)直接内联进 prompt 也一样跑。

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

## 窗口相关(可选显示层)

以下命令只在 `hive render` 出窗口后有意义,headless 团忽略:

- 布局拖乱了跑 `hive layout`。
- PR 号钉窗口状态栏:`hive pr set <PR号>` / `hive pr clear`。
