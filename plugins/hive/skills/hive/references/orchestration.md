# 编排:orch 手册

> 谁读:发起协作、拆任务的 orch,或已是别团成员、以 guest 身份编排的人。读完 SKILL.md、动手编排前读;成员干活不用读。

## 就位

orch 拆任务、写 task artifact、spawn/派发、收回信、集成终验、向 human 汇报。你不写业务代码——context 要留给验收。

- 建团者的 orch 身份按 SKILL.md 的入口规则确定:入册地址为 `<team>.orch`,成员直接寻址 `orch`。tmux 外、已是别团成员的 session 再 `hive create <新团>` 时不入册,而是以 guest 编排新团。已绑定的 tmux pane 做不到这一点:`hive create <别名>` 只会幂等返回本团。
- guest 对新团的每条 `spawn` / `kill` / `team` 都要显式加 `-t <新团>`。**派发作用域**默认仍是自己绑定的团,`hive create <新团>` 不改变绑定。看新团名册用 `hive team -t <新团>`,裸 `hive team` 仍返回原队。**回信地址**也仍是原队的 `<原team>.<你>`。例如:

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
4. **材料**:上游产出、输入数据的绝对路径。成员看不到 workflow,也不知道别人在做什么;它需要的每份材料都在这份文件里给绝对路径。

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

成员进度只看回报消息、notify 事件和 `hive team` 的 runtime 字段。前两个由 runtime 推送;收到消息后才用 `hive team` 核对状态,不用它轮询。`spawn` exit 0 且输出 `dispatched: true` 就是派发落地的凭据——不跑 `hive team` 复核名册,名册不是回执。不 `tmux capture-pane` 或任何读屏手段观察成员——残屏和中间态不是真相,还烧你的 context。已派发的任务不自己再做一遍:你的产出没人验收,还烧掉终验要用的 context。

## 成员生命周期

- 验收前不 kill:回报 ≠ 验收,不满意 `hive send` 打回追问——活成员带全部上下文,杀掉重 spawn 丢的就是它。
- 验收通过、下游任务的 artifact 写好后 `hive kill <member>`,再 spawn 下一环节。
- 唯一例外是 fix 循环(④):产出还会被打回的成员留到 verify pass 再 kill。

## Pattern library

建议模式,按任务自由组合;stage 划分、数量、顺序都是你的编排决定。

**① producer + 异构 reviewer**:改动需要独立审时,reviewer 用不同家族的 CLI,`--cli` 必须显式写。review 的 task 要求 verdict `pass`/`fail` + evidence + required-changes,并给 reviewer 和 producer 同样的原始材料路径——只让它读 producer 的报告,它就只能复述;关键结论从 diff、日志、材料自己核。

**② solo 快任务**:一个成员闭环一件小事。spawn → 回报 → 验收 → kill。

**③ explore → impl 接力**:explore 回报(摘要 + findings artifact)→ 验收 → kill explore → impl 的 task 引用 findings 路径 → spawn impl。接力棒是 artifact 文件,不是活成员;你只过目摘要,不搬运正文。

**④ fix 循环**:impl 回报后不 kill,spawn verify(task 带验收标准、branch、impl 报告路径)。fail → required-changes 打回 impl,verify 也留着复验;pass → kill 两者。默认 5 轮上限(task 可覆写),到限升级 human。

**⑤ 集成验收**:所有任务 DONE 后你自己拉集成分支、跑测试、核验收标准,过了才向 human 汇报。终验不外包。

## git / 集成纪律

单任务改动按 references/worktree.md 走。多个写码任务并行时,先建集成分支,**再 spawn 写码成员**——成员的 `hive worktree start` 才会 base 到它:

```bash
git branch <team>-integration <base>
git push -u origin <team>-integration
hive worktree set-base <team>-integration   # 写在团窗口上:在团窗口的 pane 里跑;tmux 外的 orch 让一个成员代跑
```

漏 push 时成员开 PR 会报 base 不存在,它会报给你,你补 push。

merge 只由你串行执行,一次一条。该任务验收通过且 human 批准后再运行:`gh pr merge <PR号> --match-head-commit <验过的head> --squash`——必须带 PR 号和 head,免得 pass 后又 push 的 commit 被误合。每合一条,通知 in-flight 写码成员 rebase(它们重跑 start 会拿到 `needs-rebase`)。冲突在 PR / 集成点处理,worktree 只隔离工作区,不消除冲突。首个 sub-PR 合入后可开 main PR(集成分支 → main),human review / merge 它才是最终交付。

## 对 human

- 只给已收敛结论、单个阻断问题、建议下一步——human 的注意力留给拍板。拍板用所在 CLI 的阻塞提问工具(claude 是 `AskUserQuestion`)。
- 给 human 的可执行命令各放一个 ```bash 块,块内只放命令;`hive attach <team>` 桌面 Claude 在第一次 spawn 后自己跑(SKILL.md 入口一节),不作为命令块给 human;之后每次 spawn 成功、或汇报里提到团窗口,给 `[hive <team>](#terminal/<tab_id>)` 标签指向那个 tab。
- 成员越过你直接向 human 交付:回它「终态发我」,交付线走你,验收才有着落。human 直接对某个成员改了方向:以 human 为准,更新你手里的验收标准。
- stage 汇报和最终交付要有自包含 HTML,Markdown 源同目录,发 human 时给 HTML 绝对路径;agent 间 artifact 一律 Markdown。
- 全部完成且 human 签字后才 kill 剩余成员;整团收摊用 `hive delete`。

## 窗口

布局拖乱了 `hive layout auto`;PR 号钉状态栏 `hive pr set <PR号>` / `hive pr clear`。
