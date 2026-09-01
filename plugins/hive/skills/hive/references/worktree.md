# worktree 与共享 checkout 纪律

> 谁读:领到要改仓库文件的任务时,任何成员(含 orch)动手前读这一篇;只读任务用不到。orch 的多任务集成分支纪律在 orchestration.md,以本篇为底座。

## worktree 全流程

只读任务(探索、审查、验证)直接在共享 checkout 里做——不改文件就没有隔离需求。要改文件,按下面六步走:

1. `hive worktree start <task>`(输出 JSON)。`<task>` 同时是 branch 名和 worktree 目录名:语义化 kebab-case、≤4 词、合法 branch——看名知事,别人一眼能认领。
2. 取输出 JSON 的 `path`,进入并证明入场——claude 用 `EnterWorktree path=<路径>`;codex 每条 repo 命令都把 working directory 设为该路径,并先跑 `pwd`、`git rev-parse --show-toplevel`、`git status --short --branch`。证明入场是防止改动落回共享 checkout。
3. base 解析不出就带 `--base`;`needs-rebase` 时进 worktree rebase 到提示的 base,再重跑 start——base 漂了先追平,免得 PR 带上别人的 diff。
4. 验收对象是 commit:只提交本任务范围,让派发人按 commit 判;WIP commit 可以。
5. 任务 artifact 要求开 PR 才开。实质 push、`gh pr ready`、merge 都要 human 授权——对外可见的动作由 human 把关;授权通常经 task artifact 接力(明确要求交 PR/push,或带 humanDirective)。**默认免授权的只有一步:用一个空 commit push 出 draft PR 当占位锚。**
6. 退场:claude 先 `ExitWorktree action=keep`,然后 `hive worktree done <task>`——只删 worktree,branch 留给 PR 生命周期。`done --force` 只有 human 明确 abandon 时才用,它连未合的工作一起丢。

## 共享 checkout 纪律

多人同一 checkout 时,git index、stash、branch 会互相影响:

- commit 前看 `git status --short` 和 `git diff --cached --stat`——staged 里有别人或越 scope 的文件先收敛,免得替别人提交。
- stash 前看 `git stash list`——只动自己的 stash:不 pop 别人的,也不静默 stash 别人的 untracked 文件,动了就等于改写别人的工作区。
- 并行独立 PR 用各自 worktree——在共享 checkout 里直接 branch / commit / push 会互相污染 index 和 HEAD。
