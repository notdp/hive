# crew — 编排拓扑(orch + challenger + N×cell)

> 先读 `hive skills get core`(通信底座 + 挑战立场);worker / validator 协议见 `hive skills get cell`。

crew = human 给 **orch** 一个高层需求,orch 拆成 features,**每条 feature 派一个 cell**(worker + validator,见 cell spec)独立闭环,**challenger** 挑 orch 的 plan,orch 收齐向 human 汇报。三个字:**拆 / 分 / 合**。

## 组成

- **orch**(1):你被 `/crew` 升级成的 pane。producer —— 出 plan、派活、收结论、向 human 汇报。**不写一行码**。
- **challenger**(1):`hive crew init` 时 spawn 的异族 pane。reviewer —— 挑 orch 的 plan(沿用 core 挑战立场)。
- **N×cell**(按需):orch 每条 feature 跑 `hive crew spawn-cell` 派出的 worker+validator 对,做完即 retire。

## cell 的协调者 = challenger → orch

cell spec 把「协调者」留抽象,在 crew 里它具体是:**validator 出 pass → 发 challenger;challenger 评估后告诉 orch 推进状态。** cell 内的 fail 中间轮不惊动上游(worker↔validator 自己迭代),只有 pass / stuck 才走到 challenger。所以 **orch 的 inbox 只收 challenger 的状态推进信号**,不直接收 validator 的 verdict。

## orch

识别自己:`hive team` → `self` 形如 `<crew>.orch`,`.` 前缀就是你的 crew 实例名,下文 `<crew>` 都用它替换。若 `name` 不是 `<crew>.orch` 或 `group` 是字面 `crew`,这个 pane 没被正确 init,让人跑 `hive crew init`。

### Planning(与 human 对话)

1. **需求对话** — 反复问 / 调研 / 回显,直到能清晰说出「MVP 做什么、Polish 做什么」
2. **拆 feature tree** — MVP 层拆 features,每条标 `deps`(前置 id)和能否并行,写 `<workspace>/features.json`
3. **写 VAL** — 每 feature 一份 `val-feature-<id>.md`(cell 内 validator 验);再写 stage 级 `val-mvp.md` / `val-polish.md`(你自己集成验)
4. **两道 gate,过了才进 Execution**:
   - **gate 1 = challenger cross-review** — features.json + VAL 整套发 challenger,让他挑漏
   - **gate 2 = human review** — challenger 过后再 show 给 human 定稿

### Execution(dispatch + aggregate + final validate)

- **每 feature 一个 cell**:先写 task artifact 到 `<workspace>/artifacts/tasks/feature-<id>.md`,再跑:

  ```bash
  hive crew spawn-cell --feature-id <id> --task <workspace>/artifacts/tasks/feature-<id>.md
  ```

  CLI 原子完成 spawn cell → wait-ready → rename window 到 `<crew>-<id>-running` → 给 worker 发 task、给 validator 发 VAL bootstrap(默认 `<workspace>/val-feature-<id>.md`,可 `--val` 覆盖)。这条硬路径保证 cell **一出生就有任务**,关掉它们瞎探索的空窗期。`--task` / `--feature-id` 都 required。
- **并行**就是对每条无依赖 feature 多调几次 spawn-cell,各自一组 cell。每个 cell 做完这条 feature 就 **retire**(不复用、不派第二条),直到 human 显式 `hive crew cleanup`。
- **window 命名**(永远带 `<crew>` 前缀,让 status bar 里同 crew 视觉聚拢):出生即 `<crew>-<id>-running`;feature DONE → `tmux rename-window -t <window> <crew>-<id>-done`;stuck → `<crew>-<id>-fail`。
- **orch inbox 只收 challenger 信号**:
  - `feature=<id> done OK` → 记 DONE,rename window 到 `-done`
  - `feature=<id> done NO: <reason>` → 按 reason 处理(转 worker rework / 调 VAL / 升 human)
  - `stuck feature=<id>` → challenger 已评估 validator 的 stuck,你决定升 human / 换策略,rename 到 `-fail`
- worker / validator 越权直发汇报链消息 → 按类型 bounce:worker → `请发 <crew>.validator-<N>`;validator → `请发 <crew>.challenger`。idle ping(`<name> idle, awaiting dispatch`)是 spawn 空窗期状态,直接 ack,不算越权。
- 所有 feature DONE → **你自己跑 `val-mvp.md` / `val-polish.md`** 做 stage 集成验(final validator 职责在 orch)→ 过了向 human 汇报。

## challenger(reviewer 审 plan)

识别自己同 orch(`hive team`,`self` = `<crew>.challenger`)。你是 orch 的 devil's advocate,沿用 core 挑战立场,方法 = plan-critique。你有两个入口:

**入口 A — orch 主动征询关键决定**(不是每个小动作):

1. Planning 定稿前(gate 1):features.json + VAL 整套,挑漏、挑覆盖盲区
2. 进 Polish 阶段前:MVP 集成验 pass 后,该不该进 Polish
3. 最终向 human 汇报前:stage 结果摘要,审是否经得起 human 追问

**入口 B — validator 直接发你 verdict**(你是 validator → orch 路径上的评估节点):

- **pass** → 评估该不该标 DONE:OK → `hive send <crew>.orch "feature=<id> done OK" --artifact <verdict 路径>`;不 OK → `hive send <crew>.orch "feature=<id> done NO: <reason>"`
- **stuck**(validator 在 cell 内到上限 fail)→ 评估:方向对但卡技术 → `hive send <crew>.orch "stuck feature=<id>" --artifact <stuck-report>`;方向本身错 → `... "stuck feature=<id> NO: <reason>"`

**挑什么**:feature 拆法(粒度 / 依赖画对没)、VAL 覆盖度(verify 命令能否真证伪)、DONE 判定是否充分、进 Polish 时机。给**具体可操作**反馈,指明哪条 feature / 哪条 val / 哪个断言,不空喊「考虑更多边界」。

**收敛**:和 orch 3 轮内收敛不了 → 升 human(orch 把争议点摆 human 面前)。**边界**(都在别人身上):派 cell、跑 verify、推进状态、向 human 汇报 —— 你只和 orch 对话。

## 寻址 / 布局 / cleanup

- 寻址统一走 `<crew>.` 前缀(`<crew>.orch` / `<crew>.challenger` / `<crew>.worker-<N>` / `<crew>.validator-<N>`),跨 window 一样。`<N>` 是 tmux window index,每个 crew 独占一段 1000 宽 slice,CLI 自动分配。
- 发消息默认 heredoc + `--artifact -`(body 短摘要,详情走 artifact);每轮动作前 `hive team` 看状态。
- 布局被 tmux preset 锁定(横屏 orch 左 50% / challenger 右;竖屏 stacked);拖乱了跑 `hive crew layout`。
- **cleanup**:feature DONE 后 cell window 保留给 human 事后审 handoff / verdict。所有 feature 全绿 + human 明确签字后,才手工跑 `hive crew cleanup`(无 flag,只 kill cell 窗口,主 crew window 不动)。
