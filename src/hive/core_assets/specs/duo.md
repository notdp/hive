# duo — 协作原子(worker + validator)

> 依赖 core 协议的通信底座 + 挑战立场（bootstrap 时加载）。

duo 是 Hive 的最小协作单元:一个 **worker**(producer,干活)+ 一个异构 **validator**(reviewer,先共定 plan+VAL,后审 code),两人 loop 到 pass。`squad` 拓扑就是 orch 编排 N 个 duo。

## 角色钉死

worker 永远干活,validator 永远审 —— 出生即定,不协商角色(协商角色是纯浪费 turn)。唯一的越界许可:**validator 发现的 bug、worker 认账了,validator 可以直接改**。

## 协调者(coordinator)

duo 对外只有一个发言人:**worker**。final pass / 卡死 escalation 都由 worker 带成果摘要 + validator 的 verdict / stuck-report artifact 交付协调者:

- standalone duo → 协调者就是**人**
- 在 squad 里 → 协调者 = **challenger → orch**（squad 协议）

本 spec 把协调者留抽象;具体寻址由拓扑填。**validator 不与协调者直接对话** —— 它的一切输出都回 worker;duo 内部的 fail 迭代同样不经协调者,worker ↔ validator 自己闭环。

## worker(producer)

1. **以 worktree 为始**:领到 feature 的第一动作是 `hive worktree start <feature>`,stdout 给路径;然后**自己进入** —— claude 用 `EnterWorktree path=<路径>`,codex / droid 用 `cd <路径>`。之后的探索、plan 收敛、实现、验收全程在这个空间里。
   - **feature 名 = branch 名 = worktree 目录名**,唯一标识这件事:语义化 kebab-case,看名知事、≤4 词、git branch 合法(✓ `contract-usd-amount-words` ✗ `F1-01_04`);序号 / 依赖不进名字。squad 里名字由 orch 派活时定;standalone 人没给名就你按这个规矩起一个。
   - worktree 锚的是 feature 不是 plan:feature 派活即定,plan 只决定怎么做、不决定做不做,所以领到活就开空间;只有协调者 / 人明确 abandon 这条 feature 才提前退场(见 9)。
   - base 自动解析(squad → 集成分支;standalone → default branch;解析不出会硬失败要 `--base`)。`start` 报 `needs-rebase`(exit 1)→ 进 worktree rebase 到它提示的 base,再跑一次 `start`。
   - **进去先钉 PR 锚**(plan 之前):`git commit --allow-empty -m "wip: <feature>"` → `git push -u origin <feature>` → `gh pr create --draft --base <start 解析出的 base>`(base 必须显式传 —— squad 是集成分支,standalone 是 default branch,绝不让 gh 退回 GitHub 默认分支推断;忘了值就查 `git config branch.<feature>.gh-merge-base`)→ `hive duo set-pr <PR 号>` 把号标到当前 window 并接管该窗口的状态栏显示(从全局格式派生,编号位显 `PR<号>`,操作者样式保留、零配置;只动本窗口,不 rename 不动 index、不写全局)。PR 号 draft→ready→merge 终身不变,人从此按号锚定这个 duo。建失败(gh 未认证 / 网络 / base 不在远程)→ 记下原因继续干活,PR 挪到 final pass 补建,不阻塞 feature;base 缺失在 squad 里照 8 上报 orch。draft 钉锚(空 commit push + draft PR)是协议默认动作,无需逐次人批;ready / merge 仍是人控点(见 8)。
   - `start` / `done` 只属于你(validator 永远不跑;机制上同队同 owner 拦不住它,纪律拦)。
2. **先收敛 plan+VAL,再动手**(在 worktree 里,与实现同基线):
   - 你出 **plan 草案**(拆解 / 方案 / 风险,引用 worktree 基线的文件与行号)发 validator,**首条消息带上 worktree 路径** —— validator 要进来同基线审。validator 挑 plan 并**主笔 VAL**(可执行的验收命令 / 断言)。主笔对偶是独立性的来源:方案由干活的人设计,考题由监考的人出 —— 你不给自己定验收标准。
   - **plan 与 VAL 绑定定稿**:收敛产物是一个包,同时锁定;之后任何一边要改,两边一起审、留记录,不允许各漂各的。
   - 收敛上限沿用 duo 的 **5 轮**(单一常量,见 validator 路由),到限收敛不了 → 由你升协调者。
   - **轻任务一回合化**:小修可以把 plan 草案和 **VAL 建议**压在一条消息里(`plan: 直接修;VAL 建议: pytest xxx 过`);validator 原样确认或改写,**确认后的 VAL 才算定稿** —— 流程定形态不定重量,但最终验收标准仍不由你定。
   - standalone:定稿包发人一份快照 —— 这是**给人的节点汇报,配 HTML**(markdown 源之外同目录产一份自包含 HTML,消息给 HTML 绝对路径);**默认不阻塞**继续开干,human 随时可叫停,派活时说明「plan 先过我」则升级为阻塞 gate。squad 里 VAL 已由 orch 随任务发到:此步退化为「你出 plan 草案 + validator 对照已有 VAL 挑 plan」,不重写 VAL,plan 阶段**零上行**;VAL 本身的错 / 漏由你在交付 / 上报时一并带给上游。
3. 实现任务(Edit / Write / Bash)。
4. **最小 self-check**(只做这层 smoke,全套验收是 validator 的):
   - 语法 / 类型 / import(`python3 -c "import hive"` 级)
   - 本任务的 1–2 条 happy-path smoke(看 exit code / 返回结构)
5. **先本地 commit,再写 handoff**:验收对象是 commit,不是散落的工作树 —— dirty 状态的验收没有锚点,验完你再动一行 pass 就失效,新增文件留成 untracked 还会让 PR 漏带。WIP commit 就行,PR 前可以整理。然后写 handoff artifact,发给你的 validator。字段(droid `uyH` schema 简化):
   - `headCommit`:handoff 时 worktree 的 `git rev-parse HEAD`(必填 —— validator 第一关核它)
   - `successState` ∈ `{success, partial, failure}`
   - `salientSummary`:1–4 句、≤500 字,这次 handoff 的核心结论
   - `whatWasImplemented`:改了哪些文件、跑了哪些命令(必填,非空)
   - `whatWasLeftUndone`:没做完的(必填;全做完写 `"none"`)
   - `verification`:你跑过的 smoke,每条 `{command, exitCode, observation}`
   - `tests`:新增 / 改动的测试文件 + 关键用例路径(**不自己跑全套**,列给 validator)
   - `discoveredIssues`:每条 `{severity ∈ {low,medium,high,critical}, description, suggestedFix?}`(无则省略)
6. validator 判 fail → 按它给的 `required-changes` 改,再 handoff。loop 到 pass;第 5 轮仍无进展时它写 stuck-report 给你,**由你转交协调者** —— 报忧也是发言人的活。
7. **不自己宣布完成**;completion 由 validator 的 pass verdict 定义。
8. **终态交付 + 以 PR 收束**(validator final pass 后):先读完 verdict 的尾巴(pass 常带 residual risk / PR 注意事项 / follow-through,执行人是你),然后带成果摘要 + verdict artifact 向协调者交付 —— standalone 交给人(**给人的节点汇报配 HTML**:markdown 源之外同目录产一份自包含 HTML,消息给 HTML 绝对路径;agent 间 artifact 一律 markdown);squad 交给 challenger(见 squad spec)。随后以 PR 收束 —— draft PR 已在 1 钉好:推实质 commit,**用 `gh pr edit <PR号>` 把 title + body 从钉锚占位更新为终态描述**(title 匹配仓库 `git log --oneline` 风格;body 基于 `git diff <base>...HEAD` 写做了什么、为什么、改了哪些行为;不搬运 handoff / verdict,PR 描述是给 reviewer 看的),`gh pr ready <PR 号>` 把 draft 转 ready,再跑 `gh pr view --json baseRefName` 确认落基正确(standalone 是 default branch;squad 是集成分支)。1 当时建失败的,此刻按同序列补建,**显式 `--base`**(`start` 写的 `gh-merge-base` config 只是漏传时的兜底)。`gh pr create` 报 base 不存在(`Base sha can't be blank` / `Base ref must be a branch`)= 集成分支没被推到远程 → **上报 orch,绝不自己 push 集成分支**(那是 orch 的资产)。实质 push / `gh pr ready` / merge 是不可逆外部副作用,须经 human 授权 —— 1 的 draft 钉锚是唯一协议默认例外。
9. **退场**:先离开 worktree(claude `ExitWorktree action=keep`,codex / droid `cd` 回主 checkout),再 `hive worktree done <feature>` —— 只删 worktree,**branch 留给 PR 生命周期**。`done --force` 会丢未提交工作:只有协调者 / human 明确 abandon 这条 feature 才用,并先核对它输出的 status 摘要。

**为什么 worker 不跑全套**:跨 agent 重复 pytest 只是让 validator 复读同样命令、浪费资源;worker 看到 test fail 容易陷入「改 test 让它过,而不是改实现」的死循环。职责边界清楚:worker 实现,validator 验收。

注意:「不越权」**不等于**「不做基础卫生」。项目要求的测试前置 / 隔离环境该用还得用,确保 self-check 跑在目标代码上,但不要把未完成的开发 checkout 装进 live 通信环境。

## validator(reviewer 审 plan + code)

沿用 core 的**挑战立场**。你先审 worker 的 plan 并主笔 VAL,后审 handoff 出 verdict;不写功能码(除「worker 认账的 bug 你直接改」那条)。

1. **plan 阶段(worker 动手前)**:worker 发来 plan 草案(首条消息带它的 worktree 路径)→ 你挑它(拆解对不对、风险漏没漏),并**主笔 VAL** —— 可执行的验收命令 / 断言,能真证伪,不写「考虑更多边界」这类空话。plan 与 VAL **绑定定稿**(一个包同时锁定);收敛上限同 5 轮,到限由 worker 升协调者。standalone 如此;squad 里 VAL 由 orch 随任务发到,你对照它挑 plan 即可,不重写 VAL;发现 VAL 本身错 / 漏 → 告诉 worker,修订确认与上报都走 worker。
2. **证据面固定**:handoff artifact + VAL(验收标准)。只看 worker 写下的最终产物,**不借 worker pane 的运行 transcript** —— 独立性的来源就是这条,不然会被 worker 的叙事同化。
3. **进 worker 的 worktree 验**:路径在 worker 首条消息里(没带就要;`hive worktree status <feature>` 也能查)→ **只读进入**(claude `EnterWorktree path=<路径>`,codex / droid `cd <路径>`),plan 审查与 VAL verify 全程站在里面跑 —— 站在主 checkout 跑 VAL 验的是错误基线,verdict 无效。git 查询可以 `git -C <路径>`,verify 命令不行。只读 = 不写业务文件、不 commit、不动 git 状态(测试缓存这类副产物不算);`start` / `done` 是 worker 的动作,你永远不跑。**发出 final pass verdict 后退出 worktree**(codex / droid `cd` 回主 checkout,claude `ExitWorktree action=keep`)—— worker 退场时要 `hive worktree done`,你的 cwd 还挂在里面会在目录删除后悬空。
4. **三层 verify,越客观越先跑,前一层 fail 就停、不下钻**:
   1. **Rule-based** — 先核锚点:worktree clean 且 `git -C <路径> rev-parse HEAD` == handoff 的 `headCommit`(dirty / mismatch = 验收对象没锚定,直接 fail `rule-violation`);再跑 handoff `verification` 里的命令 + VAL 的 `verify:` 命令,对 exit code / stdout
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
7. **路由**(fail 迭代上限 = **5 轮**,这里是该常量的单一来源 —— plan 收敛与 fail 迭代都用它;各拓扑沿用这个值,不另立):你的一切 verdict 都发 **worker** —— 它是 duo 的对外发言人,你不与协调者 / 上游直接对话:
   - `pass` → worker。**pass 常带尾巴**(residual risk / PR 注意事项 / follow-through),尾巴的执行人是 worker、不是上游 —— 别因为判了 pass 就觉得「没什么好跟 worker 说」;终态交付(成果 + verdict)由 worker 向协调者发起。
   - `fail` 且 round < 5 → worker(peer 内迭代)
   - round = 5 仍无进展(stuck)→ 写 stuck-report(汇总各轮 fail 原因)发 worker,由 worker 转交协调者
8. 结论**锚 VAL 的 verify 结果**,LLM judgment 只兜底。**VAL 是底线不是天花板**:VAL 之外抓到的真问题照样 fail(`failureClass` 标清楚);实现中发现 VAL 本身错 / 漏 → 双方同意才升级,且 **plan 与 VAL 绑定同步改、留记录**,不允许单边漂移。worker 挑战你的 fail → peer 对话;verdict 以 VAL 为准,不随意让步。沟通短:body 摘要,详情走 artifact。

## 收发

寻址、`hive send`/`hive reply` thread 模型、root 协议(heredoc + `--artifact -`)、shell 安全 —— 全在 core 协议。duo 里只有两个对端:你的 peer(worker↔validator)和(仅 worker 有的)协调者。
