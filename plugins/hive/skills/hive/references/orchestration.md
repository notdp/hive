# 编排:orch 手册

> 谁读:要发起协作、拆任务派人的那个 agent(orch 或 guest 编排者),读完 SKILL.md 成员章之后、动手编排之前读;成员干活不用读。

## 就位

你就是这个 team 的 **orch**:拆解任务、spawn 成员、派发、收结论、跑集成终验、向 human 汇报。你不写业务代码——orch 只是先开始派活的那个参与者,context 要留给验收。

- 还没有 team 就按 SKILL.md 动词表 `hive create`——建团者就是 orch,tmux 内外一样:你以 `<team>.orch` 入册,成员回你直接寻址 orch。
- 唯一例外:你已是别团成员时不会再入册,本团以 guest 身份编排,回信地址仍是原队的 `<原team>.<你的成员名>`——成员照抄 from 就能回到你。
- 成员的回信会注入你的对话并唤醒你,所以 spawn/派发之后照常结束 turn 等推送。然后 `hive team` 确认 `self`,开始拆解。

## 成员名就是任务标签

runtime 没有角色:spawn 时起的名字(`explore`、`impl-auth`、`review`)就是这个成员的全部身份,出现在消息地址和显示层。用语义化 kebab-case、≤4 词、看名知事。活着的成员集合就是 workflow 现状——环节推进用 spawn/kill 表达,不改名,名字一变地址和历史就对不上。

## task artifact 四件套

每个任务先写 artifact(`<workspace>/artifacts/tasks/<member>.md`;workspace 路径在 `hive team` 返回的 `runtimeWorkspace` 字段里),必含:

1. **scope**:做什么、不做什么。
2. **交付物**:形态与产出路径(报告写哪、代码交 commit 还是 PR)。
3. **验收标准**:你终验时按什么判。
4. **材料**:上游产出的 artifact 路径。成员是全盲的,不知道 workflow 形状——它需要的一切材料都在这份 artifact 里给路径,别指望它自己发现。

## spawn + 派发

```bash
hive spawn explore --task <workspace>/artifacts/tasks/explore.md
hive spawn impl-auth --cli codex --task <workspace>/artifacts/tasks/impl-auth.md
```

- `--task` 把任务作为首条 `<HIVE>` 消息原子投递——成员不会空 inbox 出生。
- `--cli` 缺省跟你同 CLI(claude|codex|grok);headless spawn 没有 pane 可参照,缺省是 claude。要异构时必须显式传,见 pattern ①。
- model 不确定就别传,默认就是对的(不照抄状态栏之类的显示串,那不是 model id)。要传时:claude 用别名 `fable` / `opus` / `sonnet`——别名永远指向该档当前最新,不会过期;典型分工:`fable` 做 verify/裁决类成员,`opus` 做执行主力(集成验收不 spawn 成员,是你自己做,见 pattern ⑤)。codex/grok 传具体 id,spawn 按该 CLI 自己的 catalog 校验,打错会带 did-you-mean 拒收。

成员完工会 `hive send` 回报你——自动锚回派发线程。读摘要,必要时读它的 artifact。

## 进度只来自回信

成员的进度信号只有三个:它的回报消息、notify 事件、`hive team` 的 runtime 字段。前两个是推送;`hive team` 用于收到消息后核对状态,不是轮询工具。派发出去之后没有待办就结束 turn,等消息唤醒。

- 禁令:不用 `tmux capture-pane` 或任何读屏手段观察成员——屏幕是给 human 看的显示层,有残屏和中间态,不是真相,读屏还烧掉你自己的 context。
- 禁令:已派发的任务不自己并行做一遍——你的产出没人验收,还烧掉终验要用的 context。

## 成员生命周期

- **验收前不 kill**。回报 ≠ 验收:不满意就 `hive send` 打回追问——活成员带全部上下文,杀掉重 spawn 会丢掉上下文。
- 验收通过、下游任务的 artifact 写好之后,`hive kill <member>`,再 spawn 下一环节。
- 唯一例外是 fix 循环(pattern ④):产出还会被打回的成员,留到下游 verify pass 再 kill。

## Pattern library

以下是建议模式,不是流水线。按任务自由组合;stage 划分、数量、顺序全是你当时的编排决定。

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

**④ fix 循环**——impl 回报后**不 kill**,spawn verify(task 带验收标准 + branch + impl 报告路径):

- verify fail → `hive send` 把 required-changes 打回给 impl——同成员带上下文修,比新成员快;verify 也留着,复验时它记得上次挂在哪。
- verify pass → kill impl + verify。
- 默认 5 轮上限(task artifact 可覆写),到限升级 human。

**⑤ 集成验收**——所有任务 DONE 后,你自己跑集成验(拉集成分支、跑测试、核验收标准)。过了才向 human 汇报。终验不外包。

**⑥ flow 脚本(机械流程)**——循环、fan-out、barrier 这类确定性控制流不用手工编排:写一个 Python 脚本交给 `hive flow run`,每个 `agent()` 都是真实成员,human 全程可见可介入。`agent()` 走的是 pane spawn,所以这条只在 tmux 里跑得起来,headless 团用不了。

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

- 跑法:把 `hive flow run workflow.py` 放进后台 shell,完成后读输出。脚本跑着时你结束当前 turn 等完成通知;期间来了消息照常处理。
- API 全貌(不需要读源码):
  - `agent(prompt, *, name, cli=None, model="") -> Member`——spawn+原子投递+阻塞等回报。prompt 就是 task artifact,写全四件套。
  - `Member` 字段:`.summary`(回报 body)、`.artifact`(回报 artifact 路径)、`.name`、`.pane`。
  - `member.ask(prompt) -> Member`——追问/打回,阻塞等回答,更新 `.summary`/`.artifact`。
  - `member.kill()`——验收后退场。
  - `parallel(*thunks) -> list`——并发跑,按调用顺序返回;任一失败等全员结束后抛 FlowError。
- 动态判断仍然手工编排;脚本只接机械流程。

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

- 只给已收敛结论、单个阻断问题、建议下一步——human 的注意力留给拍板。需要拍板用所在 CLI 的阻塞式提问工具(claude 是 `AskUserQuestion`)。
- 成员越过你直接向 human 交付时,回它「终态发我」——交付线走你,验收才有着落。
- human 直接对某个成员改了方向:以 human 为准,更新你手里的验收标准;成员回报时会说明。
- stage 汇报和最终交付要有自包含 HTML,Markdown 源和 HTML 同目录,发 human 的消息给 HTML 绝对路径;agent 间 artifact 一律 Markdown——human 看渲染,agent 读源码。
- 全部完成且 human 签字后,才 kill 剩余成员;整团收摊用 `hive delete`。

## 窗口相关(可选显示层)

以下命令只在 `hive attach` 出窗口后才有意义,headless 团直接忽略:

- 布局拖乱了跑 `hive layout`。
- PR 号钉窗口状态栏:`hive pr set <PR号>` / `hive pr clear`。
