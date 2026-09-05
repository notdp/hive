# Hive

> CLI エージェント向けの tmux ファーストな協調ランタイム — `claude` / `codex` / `grok` の各メンバーはそれぞれ自前のエンジンで動き、エンジン固有のネイティブ配信経路で `<HIVE>` メッセージをやり取りし、単一のレジストリを真実の層として共有します。

[English](README.md) · [简体中文](README.zh-CN.md) · **日本語**

_この README は英語版が正となります。翻訳は canonical 版に対して遅れることがあります。_

## Hive とは

Hive はエージェント向けのランタイムであり、人が手動で操作する CLI ではありません。チームの実体はレジストリ上の名簿（`$HIVE_HOME/state/teams/` にチームごと 1 つの JSON）とメンバーごとのエンジンであり、tmux ウィンドウはその上に `hive render` が描き出す表示層です。

この境界は実装で担保されています。tmux の外で `hive create` すれば headless なチームが登録され、表示層がなければ `hive spawn` は pane を持たないエンジンだけのメンバーを立ち上げます。受信も報告も kill も通常どおりです。ウィンドウは後から `hive render <team>` でメンバー 1 人につき 1 pane として実体化できます。tmux 側は真実を持たないため、ウィンドウを閉じても状態は失われません。

タスクの割り当て、メッセージ送信、ランタイム状態の確認はエージェントセッション内で行われ、コマンドを発行するのもエージェントです。人間側の入口はプラグインスキル `/hive:hive [team]` です。引数なしなら状況に応じて作成または参加、チーム名を渡せばそのチームに参加し、存在しなければ作成します。人が直接実行するコマンドは一部だけです: プラグインのインストール、セッショントランスクリプトの閲覧 (`hive view`)、ポップアップエディタ (`hive cvim` / `hive vim`)、分割 fork、ローカル開発のセットアップ。

## インストール

Hive は単一の Rust バイナリです。[GitHub Releases](https://github.com/notdp/hive/releases) に macOS / Linux（aarch64 と x86_64）向けのビルド済みバイナリがあります:

```bash
curl -fsSL https://github.com/notdp/hive/releases/latest/download/hive-installer.sh | sh
```

Rust ツールチェインがあれば経路がもう 2 つあります。[`cargo binstall`](https://github.com/cargo-bins/cargo-binstall) は同じビルド済み release を取得し（コンパイルなし）、`cargo install` はソースからビルドします:

```bash
cargo binstall --git https://github.com/notdp/hive hive
# または
cargo install --git https://github.com/notdp/hive hive
```

プラグイン — エージェントにプロトコルを教えるスキル — はバイナリに同梱され、`hive` が `$HIVE_HOME` 配下に実体化するローカル marketplace から配られます。1 コマンドで PATH 上のすべてのエージェント CLI に登録とインストールを行います（再実行すればインストールを修復します）:

```bash
hive plugin setup
```

内部では marketplace を実体化し、claude（2.1.229 以上）と codex それぞれに `plugin marketplace add` + install を実行します。claude 側の marketplace エントリは command source で、Claude はセッションごとに `hive plugin sync` を再実行するため、スキルの更新はバイナリに乗って届きます。codex 側のプラグインはフックを一切持ちません（フックは codex のフック審査ダイアログの後ろに置かれてしまいます）。バイナリのバージョンが変わると、hive 自身の codex 起動パスがエンジン起動前にプラグインを再 add します。リモートからは何も取得せず、settings にも触れません。

必要な環境:

- `tmux` 3.2 以上 — `hive cvim` / `hive vim` のポップアップのため。加えて `hive view` がテーマ判定に使う素の OSC 11 背景色クエリに pane が応答するのも 3.2 からです
- Rust ツールチェイン — ソースからビルドする経路でのみ必要です。インストーラはビルド済みバイナリを配ります
- `python3` — notify のポップアップが python のヒアドキュメントです（`hive flow run` にインタプリタは不要です。flow スクリプトは JavaScript で、バイナリ内蔵のエンジンが実行します）
- 少なくとも 1 つのエージェント CLI: `claude` / `codex` / `grok`

## エージェントセッションで起動する

```bash
# 初回のみ: シェルの rc に eval "$(hive shell-init zsh)" を追加
# tmux 内で hive のランチャーからエージェントを起動
$ hclaude      # もしくは: hcodex / hgrok

# エージェントセッションで以下を入力:
/hive:hive
```

エージェントは現在の pane をそのチームの orch に据え、タスクに応じてメンバーを spawn します。以降のやり取りはエージェントとの対話であり、チームはエージェントが運用します。

## `hive fork` をキーにバインドする

ターミナルのキーバインドはシェルコマンドを直接実行できないため、バインドは生の
エスケープバイトを送り、それを tmux が受け取ります。macOS + Ghostty なら
Cmd+Shift+F が ESC f を送り、tmux が fork を実行します:

```
# ~/.config/ghostty/config
keybind = cmd+shift+f=text:\x1bf

# ~/.tmux.conf
bind -n M-f run-shell -b 'hive fork --pane "#{pane_id}"'
```

`-b` を付けないと fork の実行中に tmux サーバーがブロックします。`--pane` も
必須です: バインドは pane の外から発火するため、自動検出では誤った pane を拾います。

## トランスクリプトミラーが読み取り専用である理由

対話的な Claude セッションにアタッチできる pty はありません (`claude attach` は job 専用です) が、トランスクリプトはターンの進行に合わせて 1 イベントずつ追記されるため、そのファイルを描画するものはライブミラーとなり、構造上こちらから打ち返すことはできません。`hive view` がそのレンダラーです。

claude メンバーの sessionId に対応する bg job レコードがない場合、それは対話的なセッション (デスクトップの `ccd`、join で取り込まれたセッション) であり、`hive render` はその pane にこのミラーを自動で割り当てます。ここで resume するとフォークされた job が生まれ、そのメンバー宛ての配信を奪ってしまうためです。その pane は読み取り専用です。配信自体は影響を受けません: job レコードが無いという同じ判定により、`hive send` は pane ではなく稼働中の対話セッションへ配送されます。そのメンバーに打ち込めるのは、セッションを保持しているアプリだけです。

## アップグレード

[インストール](#インストール)のインストーラ 1 行を再実行します。常に最新の release を取得します。リリースは crate のバージョンに一致する `v*` タグを push することで切られ、CI（cargo-dist）が各プラットフォームのバイナリをビルドして GitHub Release を公開します。

スキルの更新はバイナリに乗って届きます。claude 側では marketplace の command source がセッションごとに `hive plugin sync` を再実行し、変更内容を自動で拾います。codex 側では、稼働中のバイナリのバージョンに対応するエントリがキャッシュにないとき、hive の起動パスがプラグインを再 add します。プラグインの manifest バージョンは CLI のバージョンに固定されており、その固定が codex キャッシュのキーになっています。

## 開発

グローバルに入っている `hive` バイナリは、稼働中のエージェントの伝送路です。Hive 自体を開発している間はコミット済みのチェックアウトに留め、チームが使用中の汚れた worktree から `cargo install` しないこと。プラグインの実体化や hived の挙動を伴う手動検証には、使い捨ての `HIVE_HOME` / `CLAUDE_HOME` / `CODEX_HOME` と一時的な team を使い、稼働中チームの hived には触れません。テストレーンとリポジトリ規約は [AGENTS.md](AGENTS.md) を参照。

## ドキュメント

- [`docs/runtime-model.md`](docs/runtime-model.md) — レジストリと表示層のアイデンティティ、CLI ごとのネイティブランタイム源、`busy` / `inputState` / `turnPhase`
- [`docs/transcript-view.md`](docs/transcript-view.md) — `hive view` が描くもの: JSONL → `DisplayBlock` の解析モデル、ビューアのクローム、テーマ解決
- [`docs/daemon-control-socket.md`](docs/daemon-control-socket.md) — Claude supervisor daemon の制御プロトコル。その `op:"reply"` が hive の配信本線
- [`plugins/hive/skills/hive/SKILL.md`](plugins/hive/skills/hive/SKILL.md) — `/hive:hive` がエージェントに読み込ませる協調プロトコル

## ライセンス

[GPL-3.0-or-later](LICENSE) © 2026 notdp
