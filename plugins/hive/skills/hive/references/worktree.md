# worktree 与共享 checkout 纪律

> 谁读:要改仓库文件的成员(含 orch),动手前读;只读任务用不到。orch 的多任务集成纪律在 orchestration.md,以本篇为底座。

只读任务(探索、审查、验证)留在共享 checkout;要改文件按下面六步走。多人共用 checkout 时,index、stash、branch、HEAD 都是共享的,直接改文件、branch、commit、push 会互相污染。

1. `hive worktree start <task>`(输出 JSON)。`<task>` 同时是 branch 名和 worktree 目录名:语义化 kebab-case、≤4 词、合法 branch——看名知事。
2. 进入输出 JSON 的 `path`,按引擎选择做法:

   - 只对 claude:用 `EnterWorktree path=<路径>` 切换 session cwd,让相对路径的 Edit/Write 落进 worktree。第 6 步的 ExitWorktree 以此为前提。工具不在或不可用时,按下一条做。
   - codex/grok:把每条 repo 命令的工作目录设为该路径。

   所有引擎进入后都用一条命令证明入场:`pwd && git rev-parse --show-toplevel && git status --short --branch`,三个输出都指向 worktree 才动第一个文件,否则改动会落回共享 checkout。入场证明只做一次;改完不用再核共享 checkout,因为修改始终在 worktree 内。

3. base 解析不出就带 `--base`;返回 `needs-rebase` 时进 worktree rebase 到提示的 base,再重跑 start——base 漂了先追平,免得 PR 带上别人的 diff。
4. 验收对象是 commit:只提交本任务范围,让派发人按 commit 判;WIP commit 可以。任务说了不用 commit 就不 commit,交修改路径和验证结果。
5. 任务 artifact 要求开 PR 才开。实质 push、`gh pr ready`、merge 都要 human 授权,通常经 task artifact 接力(明确要求交 PR/push,或带 humanDirective)。**默认免授权的只有一步:空 commit push 出 draft PR 当占位锚。**
6. 派发人验过、或让你收摊后再退场。运行 `hive worktree done <task>`,只删 worktree,branch 留给 PR 生命周期。只对 claude:done 前先 `ExitWorktree action=keep`。人还在 worktree 里或有未提交改动时,`done` 会拒绝。worktree 是交付证据,不要提前删。被打回要再改就重跑 `start`,同一 branch 会挂回同一路径。`done --force` 只在 human 明确 abandon 时用,它连未合的工作一起丢。

不得不在共享 checkout 里动 git 时:commit 前看 `git status --short` 和 `git diff --cached --stat`,staged 里有别人或越 scope 的文件先收敛;stash 前看 `git stash list`,只动自己的 stash——不 pop 别人的,也不静默 stash 别人的 untracked 文件。并行独立 PR 用各自 worktree。
