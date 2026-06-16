from pathlib import Path


def test_hive_skill_guides_multiline_send_via_artifact():
    """守 canonical heredoc + stdin artifact 片段(root 协议 happy path)。

    stub 学 agent-browser:只留 install happy-path 一行,upgrade / 本地刷新等变体
    指回 README(canonical 由 test_hive_install_docs_* 守)。消息协议(含
    heredoc/artifact idiom)随 CLI 下发,锚在 core spec。只断言低变更、高价值的
    shell idioms,不守具体中文措辞。"""
    repo_root = Path(__file__).resolve().parents[2]
    stub_text = (repo_root / "skills" / "hive" / "SKILL.md").read_text()
    core_spec = (repo_root / "src" / "hive" / "core_assets" / "specs" / "core.md").read_text()

    # stub keeps only the install happy-path; upgrade/refresh variants live in README
    assert 'npx skills add https://github.com/notdp/hive -g --all' in stub_text

    # stub is a discovery pointer at the CLI-shipped core spec
    assert "hive skills get core" in stub_text

    # heredoc + stdin artifact idiom now lives in the version-locked core spec
    assert "--artifact -" in core_spec
    assert "<<'EOF'" in core_spec
    assert "--artifact - <<'EOF'" in core_spec


def test_role_content_is_cli_served_specs_not_installed_skills():
    """拓扑重构终态:角色内容不再是装机 skill,而是 CLI 现取的 spec
    (`hive skills get <role>`)。每个角色 spec 都在;worker/validator 指 duo
    内核、orch/challenger 指 squad;fat handoff schema 单一来源在 duo spec。"""
    specs = Path(__file__).resolve().parents[2] / "src" / "hive" / "core_assets" / "specs"

    for role in ("squad-orch", "squad-challenger", "squad-worker", "squad-validator",
                 "duo-worker", "duo-validator"):
        assert (specs / f"{role}.md").exists(), f"missing role spec: {role}.md"

    assert "hive skills get squad" in (specs / "squad-orch.md").read_text()
    assert "hive skills get squad" in (specs / "squad-challenger.md").read_text()
    for role in ("squad-worker", "duo-worker", "duo-validator"):
        assert "hive skills get duo" in (specs / f"{role}.md").read_text()

    # fat kernel (handoff schema) single-sourced in the duo spec, never copied
    assert "salientSummary" in (specs / "duo.md").read_text()
    for role in ("squad-worker", "squad-validator", "duo-worker", "duo-validator"):
        assert "salientSummary" not in (specs / f"{role}.md").read_text()


def test_hive_is_the_only_installed_skill():
    """单一装机 skill = `/hive`;duo/squad + 所有角色都被拉进 CLI(`hive skills
    get`),不再是 SKILL.md —— 可 drift 面收到只剩 /hive。"""
    skills = Path(__file__).resolve().parents[2] / "skills"
    installed = sorted(p.name for p in skills.iterdir() if p.is_dir())
    assert installed == ["hive"], f"expected only the hive skill installed, got {installed}"


def test_hive_picker_dispatches_to_cli_init():
    """/hive 起拓扑时问 duo/squad(引用 core「问用户」),按答案跑 CLI 的
    `hive duo init` / `hive squad init`,不再 dispatch /duo · /squad skill。"""
    stub = (Path(__file__).resolve().parents[2] / "skills" / "hive" / "SKILL.md").read_text()
    assert "hive duo init" in stub
    assert "hive squad init" in stub
    assert "问用户" in stub


def _section(text: str, start: str, end: str) -> str:
    start_index = text.index(start)
    end_index = text.index(end, start_index + len(start))
    return text[start_index:end_index]


def test_hive_install_docs_keep_live_transport_out_of_dev_refresh():
    """守 live/dev lane 边界,只断言高风险命令和 source-test 入口。"""
    repo_root = Path(__file__).resolve().parents[2]
    readme_text = (repo_root / "README.md").read_text()
    agents_text = (repo_root / "AGENTS.md").read_text()
    agents_build = _section(agents_text, "## Build, Test, and Development Commands", "## Coding Style")
    readme_contributors = _section(readme_text, "## For Contributors", "## Docs")
    toxic = (
        "python3 -m pip install -e . --break-system-packages",
        'npx skills add "$PWD" -g --all',
    )

    assert "pipx upgrade hive" in readme_text
    assert "npx skills update hive -g" in readme_text
    assert 'npx skills add https://github.com/notdp/hive -g --all' in readme_text
    for command in toxic:
        assert command not in agents_build
        assert command not in readme_contributors
    assert "PYTHONPATH=src python -m pytest" in agents_build
    assert "PYTHONPATH=src python -m pytest" in readme_contributors


def test_specs_point_onward_only_via_reachable_skills_get():
    """A CLI-served spec may point onward only via `hive skills get <name>` — never
    at skill-home files (`references/...`) or a stub section (`../SKILL.md`) that an
    agent holding just the CLI output cannot follow. Guards the regression where
    slimming the stub left specs pointing at a `消息机制` section it no longer has."""
    specs = Path(__file__).resolve().parents[2] / "src" / "hive" / "core_assets" / "specs"
    for p in sorted(specs.glob("*.md")):
        text = p.read_text()
        assert "references/" not in text, f"{p.name}: unreachable references/ pointer"
        assert "../SKILL.md" not in text, f"{p.name}: points at ../SKILL.md (stub has no protocol sections)"


def test_stub_tells_init_runner_to_execute_next_itself():
    """init's role load reaches the current pane via the JSON `next` field the
    agent runs itself — the stub must say so and never regress to the old
    "自动到位" wording (which described the fake-user-message injection)."""
    repo_root = Path(__file__).resolve().parents[2]
    stub_text = (repo_root / "skills" / "hive" / "SKILL.md").read_text()
    assert "自动到位" not in stub_text
    assert "`next`" in stub_text
    assert "hive skills get duo-worker" in stub_text


def _spec(name: str) -> str:
    specs = Path(__file__).resolve().parents[2] / "src" / "hive" / "core_assets" / "specs"
    return (specs / f"{name}.md").read_text()


def test_duo_worktree_is_feature_anchored_not_plan_anchored():
    """worktree 锚 feature 不锚 plan:领活第一动作就是 start,plan 在 worktree 里
    与实现同基线收敛。守住旧「plan 定稿后才开 worktree / 主 checkout 纯文本」
    不回归 —— 那个时序让 plan 阶段站在错误基线上(live squad 实测翻车点)。"""
    duo = _spec("duo")
    assert "主 checkout 纯文本" not in duo
    assert "不开 worktree" not in duo
    assert "以 worktree 为始" in duo
    assert "领到 feature 的第一动作是 `hive worktree start" in duo


def test_duo_validator_must_stand_inside_the_worktree():
    """validator 与 worker 同基线:进 worktree 验,git -C 不能替代(站主 checkout
    跑 VAL 验的是错误基线);final pass 后退出,worker 才能 done 干净退场。"""
    duo = _spec("duo")
    assert "或不进去直接" not in duo  # the old git -C escape hatch
    assert "只读进入" in duo
    assert "退出 worktree" in duo


def test_validator_routes_everything_to_worker():
    """单发言人拓扑:validator 一切 verdict 都回 worker,worker 终态交付上游。
    守住旧「pass → challenger」直发路由不回归 —— 它让 pass 尾巴绕过执行人,
    还给 challenger 留了把 plan pass 误标 DONE 的口子。"""
    squad_validator = _spec("squad-validator")
    assert "hive send <squad>.challenger" not in squad_validator
    assert 'hive send <squad>.worker-<N> "verdict' in squad_validator
    # duo kernel: the coordinator talks to worker only, never to validator
    assert "对外发言人" in _spec("duo")


def test_squad_challenger_entry_b_is_worker_terminal_delivery():
    """challenger 入口 B 收 worker 的终态交付(成果 + verdict artifact),不收
    validator 直发;plan 阶段零上行。"""
    squad = _spec("squad")
    challenger = _spec("squad-challenger")
    assert "worker 的终态交付" in squad
    assert "worker 的终态交付" in challenger
    assert "validator 直接发你 verdict" not in squad
    assert "请发你的 worker" in squad  # bounce guidance for validator strays


def test_squad_orch_pushes_integration_branch_to_origin():
    """集成分支是 orch 的资产:建 / 推远程 / 登记一套动作。GitHub PR 的 base 必须
    在远程存在 —— 漏 push 时 worker 上报而不是代推(live squad 实测翻车点)。"""
    squad = _spec("squad")
    assert "git push -u origin <squad>-integration" in squad
    assert "不自己 push 集成分支" in _spec("squad-worker")


def test_standalone_plan_snapshot_is_human_facing_html():
    """human-facing 的三类节点汇报(plan 快照 / final / stage)都要 markdown 源 +
    自包含 HTML;plan 快照是最容易漏的那个(round-1 验收实抓)。"""
    duo = _spec("duo")
    snapshot_lines = [l for l in duo.splitlines() if "快照" in l and "standalone" in l]
    assert snapshot_lines, "standalone plan snapshot clause missing from duo.md"
    assert any("HTML" in l and "绝对路径" in l for l in snapshot_lines), (
        "standalone plan snapshot must carry the human-facing HTML requirement"
    )


def test_duo_pins_draft_pr_anchor_right_after_worktree_start():
    """draft PR 钉锚:进 worktree 即 空commit→push→draft(显式 base,禁默认分支
    推断)→`hive duo set-pr`;final pass 推实质 commit + `gh pr ready`。"""
    duo = _spec("duo")
    assert "--allow-empty" in duo
    assert "git push -u origin" in duo
    assert "gh pr create --draft --base" in duo
    assert "hive duo set-pr" in duo
    assert "gh pr ready" in duo


def test_set_pr_owns_display_no_operator_config():
    """set-pr 原生接管窗口状态栏显示;README 不再教用户配 window-status-format
    (operator tmux.conf 方案被 human 否决,不回归)。"""
    assert "hive duo set-pr" in _spec("duo-worker")
    assert "hive duo set-pr" in _spec("squad")
    readme = (Path(__file__).resolve().parents[2] / "README.md").read_text()
    assert "window-status-format" not in readme


def test_duo_worker_clarifies_ambiguous_human_tasks_first():
    """需求澄清 gate:人类对话语境的模糊任务先用阻塞式提问工具钉死需求;
    带完整 task artifact + VAL 的派活不加提问环。"""
    duo_worker = _spec("duo-worker")
    assert "AskUserQuestion" in duo_worker
    assert "request_user_input" in duo_worker
    assert "task artifact" in duo_worker


def test_non_bootstrap_cross_refs_are_prose_not_commands():
    """spec 正文里的 cross-ref 用 prose 形式（"core「没活干时」"、"duo 内核"），
    不用 command 形式（`hive skills get core`）——后者会让 agent 每 turn 重取。
    bootstrap 段和 debug/advanced-routing 按需引用除外。"""
    specs = Path(__file__).resolve().parents[2] / "src" / "hive" / "core_assets" / "specs"
    import re
    toxic = re.compile(r'(?:沿用|统一见|全在|见) `hive skills get (?:core|duo|squad)`')
    blockquote_cmd = re.compile(r'^>\s*先读\s*`hive skills get')
    for p in sorted(specs.glob("*.md")):
        for i, line in enumerate(p.read_text().splitlines(), 1):
            assert not toxic.search(line), (
                f"{p.name}:{i}: command-form cross-ref in prose — use prose label instead"
            )
            assert not blockquote_cmd.search(line), (
                f"{p.name}:{i}: blockquote command triggers re-read — use dependency statement"
            )


def test_bootstrap_sections_are_marked_once():
    """bootstrap 段的标题明确标为"首 turn 执行一次"或等价表述，
    避免 agent 每 turn 重跑。"""
    specs = Path(__file__).resolve().parents[2] / "src" / "hive" / "core_assets" / "specs"
    for name in ("duo-validator", "squad-orch", "squad-challenger"):
        text = (specs / f"{name}.md").read_text()
        assert "首 turn" in text, (
            f"{name}.md: bootstrap section should indicate first-turn-only"
        )
    for name in ("duo-worker", "squad-worker", "squad-validator"):
        text = (specs / f"{name}.md").read_text()
        assert "首 turn 执行一次" in text, (
            f"{name}.md: bootstrap code block should indicate first-turn-only"
        )


def test_handoff_is_anchored_to_a_local_commit():
    """验收对象是 commit 不是散落工作树:worker handoff 前先本地 commit 并报
    headCommit;validator rule-based 第一关核 clean + HEAD 一致,dirty 直接 fail。
    这也是 squad merge queue `--match-head-commit` 的上游锚点。"""
    duo = _spec("duo")
    assert "先本地 commit" in duo
    assert "headCommit" in duo
    assert "rev-parse HEAD" in duo
    assert "先本地 commit" in _spec("squad-worker")
