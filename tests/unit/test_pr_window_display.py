"""Tests for `_derive_pr_window_status` — the per-window status-format
derivation behind `hive duo set-pr`.

The helper rewrites the *global* window-status format so the index position
renders `PR<n>` for windows stamped with `@hive-pr`. Skip semantics
(returning None) are part of the contract: skip when the user already wired
`@hive-pr` into their global format, and when there is no replaceable `#I`.
`##I` is tmux's escaped literal `#I` — never rewritten; the pathological
`###I` triple is intentionally unsupported (conservative no-replace beats
corrupting a user's format).
"""

from hive.cli import _PR_INDEX_TOKEN, _derive_pr_window_status


def test_derives_plain_padded_format():
    assert (
        _derive_pr_window_status("  #I #W  ")
        == "  #{?#{@hive-pr},PR#{@hive-pr},#I} #W  "
    )


def test_preserves_style_wrappers_and_padding():
    derived = _derive_pr_window_status("#[bg=yellow,fg=black,bold]  #I #W  #[default]")
    assert derived == (
        "#[bg=yellow,fg=black,bold]  #{?#{@hive-pr},PR#{@hive-pr},#I} #W  #[default]"
    )


def test_derives_tmux_default_format():
    derived = _derive_pr_window_status("#I:#W#{?window_flags,#{window_flags}, }")
    assert derived == "#{?#{@hive-pr},PR#{@hive-pr},#I}:#W#{?window_flags,#{window_flags}, }"


def test_skips_when_global_already_references_hive_pr():
    assert _derive_pr_window_status("#{?#{@hive-pr},PR#{@hive-pr},#I}:#W") is None


def test_skips_when_no_index_token():
    assert _derive_pr_window_status("#W only") is None


def test_skips_empty_or_missing_global():
    assert _derive_pr_window_status(None) is None
    assert _derive_pr_window_status("") is None


def test_escaped_literal_hash_i_is_not_rewritten():
    # `##I` renders a literal `#I` — not a replaceable index token, so skip.
    assert _derive_pr_window_status("##I #W") is None


def test_replaces_real_tokens_and_leaves_escaped_ones():
    derived = _derive_pr_window_status("#I #W ##I #I")
    assert derived == f"{_PR_INDEX_TOKEN} #W ##I {_PR_INDEX_TOKEN}"
