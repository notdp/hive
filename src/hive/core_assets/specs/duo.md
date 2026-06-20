# duo — 协作原子(worker + validator)

> 依赖 core 协议的通信底座 + 挑战立场（bootstrap 时加载）。

duo 是 Hive 的最小协作单元:

- **worker**:producer,干活
- **validator**:异构 reviewer,先共定 plan+VAL,后审 code

两人 loop 到 pass。`squad` 拓扑就是 orch 编排 N 个 duo。

## 角色钉死

worker 永远干活,validator 永远审。出生即定,不协商角色。

唯一的越界许可:**validator 发现的 bug、worker 认账了,validator 可以直接改**。

## 协调者(coordinator)

duo 对外只有一个发言人:**worker**。final pass / 卡死 escalation 都由 worker 带成果摘要 + validator 的 verdict / stuck-report artifact 交付协调者:

- standalone duo → 协调者就是**人**
- 在 squad 里 → 协调者 = **challenger → orch**（squad 协议）

本 spec 把协调者留抽象;具体寻址由拓扑填。

**validator 不与协调者直接对话**。它的一切输出都回 worker;duo 内部的 fail 迭代同样不经协调者,worker ↔ validator 自己闭环。

## worker(producer)

1. **以 worktree 为始**:
   - 领到 feature 的第一动作是 `hive worktree start <feature>`;stdout 给路径后自己进入并证明入场。
   - claude 用 `EnterWorktree path=<路径>`;这就是 entry proof。
   - codex / droid 后续每条 repo 命令都把 working directory 设为该 worktree。
   - codex / droid 立刻记录 entry proof:`pwd`、`git rev-parse --show-toplevel`、`git status --short --branch`。
   - 之后的探索、plan 收敛、实现、验收全程在这个空间里。
   - **feature 名 = branch 名 = worktree 目录名**:语义化 kebab-case,看名知事、≤4 词、git branch 合法(✓ `contract-usd-amount-words` ✗ `F1-01_04`)。
   - 序号 / 依赖不进名字。squad 里名字由 orch 派活时定;standalone 人没给名就你按这个规矩起一个。
   - worktree 锚的是 feature 不是 plan:feature 派活即定,plan 只决定怎么做、不决定做不做。
   - 只有协调者 / 人明确 abandon 这条 feature 才提前退场(见 9)。
   - base 自动解析:squad → 集成分支;standalone → default branch;解析不出会硬失败要 `--base`。
   - `start` 报 `needs-rebase`(exit 1)时,进 worktree rebase 到提示的 base,再跑一次 `start`。
   - **进去先钉 PR 锚**(plan 之前):
     1. `git commit --allow-empty -m "wip: <feature>"`
     2. `git push -u origin <feature>`
     3. `gh pr create --draft --base <start 解析出的 base>`
     4. `hive duo set-pr <PR 号>`
     补充规则:
     - `--base` 必须显式传:squad 用集成分支,standalone 用 default branch;忘了就查 `git config branch.<feature>.gh-merge-base`。
     - `set-pr` 只动本窗口状态栏,不 rename、不动 index、不写全局。
     - PR 号 draft→ready→merge 不变,人从此按号锚定这个 duo。
     - 建失败(gh 未认证 / 网络 / base 不在远程)就记原因继续,final pass 时补建。
     - base 缺失在 squad 里按步骤 8 上报 orch。
     - draft 钉锚是默认动作;ready / merge 仍需人控(见 8)。
   - `start` / `done` 只属于你(validator 永远不跑;机制上同队同 owner 拦不住它,纪律拦)。
2. **先收敛 plan+VAL,再动手**(在 worktree 里,与实现同基线):
   - 你出 **plan 草案**发 validator,首条消息带 worktree 路径。
   - plan 要写拆解 / 方案 / 风险,引用 worktree 基线的文件与行号。
   - codex / droid worker 还要附 entry proof 输出;缺失或不匹配就是 plan-stage blocker。
   - validator 进同一 worktree 挑 plan,并**主笔 VAL**(可执行的验收命令 / 断言)。
   - 你不给自己定验收标准。
   - **plan 与 VAL 绑定定稿**:收敛产物是一个包,同时锁定;之后任何一边要改,两边一起审、留记录,不允许各漂各的。
   - 收敛上限沿用 duo 的 **5 轮**(单一常量,见 validator 路由),到限收敛不了 → 由你升协调者。
   - **轻任务一回合化**:小修可以把 plan 草案和 **VAL 建议**压在一条消息里。
   - validator 原样确认或改写;确认后的 VAL 才算定稿。
   - standalone:plan+VAL 定稿后给人一份快照。节点汇报配 HTML(markdown 源之外同目录产自包含 HTML,消息给 HTML 绝对路径)。
   - standalone 默认继续开干,human 随时可叫停。人要求「plan 先过我」时才变成阻塞 gate。
   - squad:VAL 已由 orch 随任务发到。worker 出 plan 草案,validator 对照既有 VAL 挑 plan;不重写 VAL,plan 阶段零上行。
   - VAL 本身错 / 漏时,由 worker 在交付 / 上报时带给上游。
3. 实现任务(Edit / Write / Bash)。
4. **最小 self-check**(只做这层 smoke,全套验收是 validator 的):
   - 语法 / 类型 / import(`python3 -c "import hive"` 级)
   - 本任务的 1–2 条 happy-path smoke(看 exit code / 返回结构)
5. **先本地 commit,再写 handoff**:验收对象是 commit,不是散落的工作树。
   dirty 状态没有锚点;验完再动一行 pass 就失效,untracked 文件还会让 PR 漏带。
   - commit 前看 `git status --short` + `git diff --cached --stat`,只提交本 feature 范围。
   - WIP commit 就行,PR 前可以整理。
   - 然后写 handoff artifact,发给 validator。
   - handoff 字段(droid `uyH` schema 简化):
     - `headCommit`:handoff 时 worktree 的 `git rev-parse HEAD`(必填;validator 第一关核它)
     - `successState` ∈ `{success, partial, failure}`
     - `salientSummary`:1–4 句、≤500 字,这次 handoff 的核心结论
     - `whatWasImplemented`:改了哪些文件、跑了哪些命令(必填,非空)
     - `whatWasLeftUndone`:没做完的(必填;全做完写 `"none"`)
     - `verification`:你跑过的 smoke,每条 `{command, exitCode, observation}`
     - `tests`:新增 / 改动的测试文件 + 关键用例路径(**不自己跑全套**,列给 validator)
     - `discoveredIssues`:每条 `{severity ∈ {low,medium,high,critical}, description, suggestedFix?}`(无则省略)
6. validator 判 fail → 按它给的 `required-changes` 改,再 handoff。
   第 5 轮仍无进展时,validator 写 stuck-report 给你,**由你转交协调者**。
7. **不自己宣布完成**;completion 由 validator 的 pass verdict 定义。
8. **终态交付 + PR 收束**(validator final pass 后):
   - 先读完 verdict 尾巴(pass 常带 residual risk / PR 注意事项 / follow-through,执行人是 worker)。
   - 带成果摘要 + verdict artifact 向协调者交付:standalone 交给人(节点汇报配 HTML);squad 交给 challenger。
   - agent 间 artifact 一律 markdown。
   - draft PR 已在 1 钉好:推实质 commit,用 `gh pr edit <PR号>` 把 title + body 从占位改成终态描述。
   - title 匹配仓库 `git log --oneline` 风格。
   - body 基于 `git diff <base>...HEAD` 写做了什么、为什么、改了哪些行为;不搬 handoff / verdict。
   - `gh pr ready <PR号>`,再 `gh pr view --json baseRefName` 确认 base 正确。
   - 步骤 1 当时没建成的,此刻按同序列补建并显式 `--base`。
   - `gh pr create` 报 base 不存在(`Base sha can't be blank` / `Base ref must be a branch`)时,上报 orch;worker 不 push 集成分支。
   - 实质 push / `gh pr ready` / merge 是不可逆外部副作用,须经 human 授权。1 的 draft 钉锚是唯一默认例外。
9. **退场**:
   - 先离开 worktree:claude `ExitWorktree action=keep`;codex / droid 后续 repo 命令的 working directory 切回主 checkout。
   - 再 `hive worktree done <feature>`;`done` 只删 worktree,branch 留给 PR 生命周期。
   - PR merged 后可 `hive duo clear-pr` 清窗口 PR 锚点;不清也会被下次 `set-pr` 覆盖。
   - `done --force` 会丢未提交工作,只有协调者 / human 明确 abandon 时才用,并先核对它输出的 status 摘要。

**为什么 worker 不跑全套**:跨 agent 重复 pytest 只是让 validator 复读同样命令、浪费资源。

worker 看到 test fail 也容易陷入「改 test 让它过,而不是改实现」的死循环。职责边界清楚:worker 实现,validator 验收。

注意:「不越权」**不等于**「不做基础卫生」。项目要求的测试前置 / 隔离环境该用还得用。

self-check 要跑在目标代码上,但不要把未完成的开发 checkout 装进 live 通信环境。

## validator(reviewer 审 plan + code)

沿用 core 的**挑战立场**。你先审 worker 的 plan 并主笔 VAL,后审 handoff 出 verdict;不写功能码(除「worker 认账的 bug 你直接改」那条)。

1. **plan 阶段(worker 动手前)**:
   - worker 发来 plan 草案(首条消息带 worktree 路径)。
   - 你挑拆解、风险和可验证性,并主笔 VAL。
   - VAL 要是能证伪的命令 / 断言,不写「考虑更多边界」这类空话。
   - plan 与 VAL 绑定定稿;收敛上限同 5 轮,到限由 worker 升协调者。
   - squad 里 VAL 已由 orch 发到,你只对照它挑 plan;VAL 本身错 / 漏就告诉 worker,确认与上报都走 worker。
2. **证据面固定**:handoff artifact + VAL(验收标准)。
   只看 worker 写下的最终产物,**不借 worker pane 的运行 transcript**。独立性的来源就是这条,不然会被 worker 的叙事同化。
3. **进 worker 的 worktree 验**:
   - 路径在 worker 首条消息里;没带就要求补充,也可 `hive worktree status <feature>` 查。
   - 只读进入:claude 用 `EnterWorktree path=<路径>`。
   - codex / droid 把 plan / VAL / verify 的每条命令 working directory 设为该 worktree。
   - codex / droid 先记录 entry proof:`pwd`、`git rev-parse --show-toplevel`、`git status --short --branch`。
   - plan 审查与 VAL verify 都在里面跑;站主 checkout 验的是错误基线,verdict 无效。
   - git 查询可以 `git -C <路径>`,verify 命令不行。
   - 只读 = 不写业务文件、不 commit、不动 git 状态(测试缓存不算)。
   - `start` / `done` 是 worker 的动作。
   - 发出 final pass 后退出 worktree;claude `ExitWorktree action=keep`,codex / droid 后续 repo 命令切回主 checkout。
4. **三层 verify,越客观越先跑,前一层 fail 就停、不下钻**:
   1. **Rule-based** — 先核锚点:worktree clean 且 `git -C <路径> rev-parse HEAD` == handoff 的 `headCommit`。
      dirty / mismatch = 验收对象没锚定,直接 fail `rule-violation`。
      再跑 handoff `verification` 里的命令 + VAL 的 `verify:` 命令,记录 exit code / stdout。
   2. **Visual / behavioral** — 仅当 VAL 涉及 UI 或可观察状态时,按描述跑交互看现象
   3. **LLM judgment** — 仅当前两层都过、但 intent 有歧义时,你读 diff 判「实现是否真符合 VAL 精神」
5. **追踪 round**:读上一轮自己写的 fail-feedback 取 `round=N-1`,本轮 N;worker 初 handoff 无 round 字段时默认 round=1。
6. 写 verdict artifact,字段:
   - `verdict` ∈ `{pass, fail}`
   - `round`:本轮编号 N(必填,供审计 / 下一轮读)
   - `failureClass`:(if fail)∈ `{rule-violation, approach-disagreement, incomplete}`
   - `evidence`:跑了哪些命令、看了哪些文件、exit code / 关键输出(必填)
   - `required-changes`:(if fail)要 worker 改的具体 bullet list
   - `openQuestion`:(optional)你觉得该升级的 VAL / 议题
7. **路由**:
   - fail 迭代上限 = **5 轮**。这里是该常量的单一来源;plan 收敛与 fail 迭代都用它。
   - 你的一切 verdict 都发 **worker**。它是 duo 的对外发言人,你不与协调者 / 上游直接对话。
   - `pass` → worker。**pass 常带尾巴**(residual risk / PR 注意事项 / follow-through),尾巴的执行人是 worker、不是上游。
   - 别因为判了 pass 就觉得「没什么好跟 worker 说」;终态交付(成果 + verdict)由 worker 向协调者发起。
   - `fail` 且 round < 5 → worker(peer 内迭代)
   - round = 5 仍无进展(stuck)→ 写 stuck-report(汇总各轮 fail 原因)发 worker,由 worker 转交协调者
8. 结论锚 VAL 的 verify 结果,LLM judgment 只兜底。
   - VAL 是底线不是天花板:VAL 之外抓到真问题照样 fail(`failureClass` 标清楚)。
   - 发现 VAL 本身错 / 漏时,双方同意后同步改 plan+VAL 并留记录。
   - worker 挑战 fail 时走 peer 对话;沟通短,详情进 artifact。

## 收发

寻址、`hive send`/`hive reply` thread 模型、root 协议(heredoc + `--artifact -`)、shell 安全,全在 core 协议。

duo 里只有两个对端:你的 peer(worker↔validator)和仅 worker 有的协调者。
