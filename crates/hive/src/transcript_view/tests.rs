use super::*;
use serde_json::{json, Value};

fn row_at(kind: &str, content: Value, usage: Option<Value>, ts: Option<&str>) -> String {
    let mut msg = json!({ "content": content });
    if let Some(u) = usage {
        msg["usage"] = u;
    }
    let mut row = json!({ "type": kind, "message": msg });
    if let Some(t) = ts {
        row["timestamp"] = json!(t);
    }
    row.to_string()
}

fn row(kind: &str, content: Value, usage: Option<Value>) -> String {
    row_at(kind, content, usage, None)
}

fn tool_use(name: &str, id: &str, input: Value) -> String {
    row(
        "assistant",
        json!([{"type": "tool_use", "id": id, "name": name, "input": input}]),
        None,
    )
}

fn tool_result(id: &str, content: Value, is_error: bool) -> String {
    row(
        "user",
        json!([{"type": "tool_result", "tool_use_id": id,
                "content": content, "is_error": is_error}]),
        None,
    )
}

fn text(kind: &str, body: &str) -> String {
    row(kind, json!([{"type": "text", "text": body}]), None)
}

// ---- plain-stream tests ---------------------------------------------

#[test]
fn test_assistant_text_renders_with_marker_and_markdown() {
    let mut p = StreamPrinter::new();
    let out = p
        .push_rendered(&text("assistant", "done: **all green**"))
        .unwrap();
    assert!(out.contains("⏺"), "{out}");
    // grok markdown engine: bold content survives, markers are hidden
    assert!(out.contains("all green"), "{out}");
    assert!(!out.contains("**"), "{out}");
    assert!(!p.working);
}

#[test]
fn test_tool_use_prefers_the_human_readable_hint() {
    let mut p = StreamPrinter::new();
    let pushed = p.push_rendered(&tool_use(
        "Bash",
        "t1",
        json!({"command": "ls", "description": "List files"}),
    ));
    assert!(pushed.is_none(), "runs finalize late");
    assert!(p.working);
    let out = p.flush_rendered().unwrap();
    assert!(out.contains("Bash") && out.contains("List files"));
    assert!(!out.replace("List files", "").contains("ls"));
}

#[test]
fn test_table_frame_is_one_style() {
    let t = &crate::view_theme::GROKDAY;
    let md = "| 键 | 作用 |\n|---|---|\n| Ctrl+B | 后台 |";
    let lines = grok_md::render_ratatui(md, t, 60);
    const BOX: &str = "─│┌┐└┘├┤┬┴┼";
    let mut seen = 0;
    for line in &lines {
        for span in &line.spans {
            if span.content.is_empty() || !span.content.chars().all(|c| BOX.contains(c)) {
                continue;
            }
            seen += 1;
            // The engine hands verticals a dim muted style and horizontals
            // none at all; both must come out the same or the column
            // flickers where the rules cross it.
            assert_eq!(span.style.fg, Some(t.md_muted), "{:?}", span.content);
            assert!(
                !span
                    .style
                    .add_modifier
                    .contains(ratatui::style::Modifier::DIM),
                "{:?} still dim",
                span.content
            );
        }
    }
    assert!(seen >= 6, "expected a framed table, saw {seen} frame spans");
}

#[test]
fn test_a_picture_becomes_an_inline_chip() {
    let mut p = TranscriptParser::new();
    let data = "A".repeat(4000); // ~3 KB decoded
    let out = p.push(&row(
        "user",
        json!([{"type": "image", "source": {"type": "base64", "media_type": "image/webp", "data": data}}]),
        None,
    ));
    match &out[0] {
        DisplayBlock::User(u) => assert_eq!(u.text, "[Image #1]"),
        other => panic!("expected User, got {other:?}"),
    }
}

#[test]
fn test_a_picture_and_its_words_are_one_band() {
    let mut p = TranscriptParser::new();
    let out = p.push(&row(
        "user",
        json!([
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "A".repeat(400)}},
            {"type": "text", "text": "这张图里排版是不是有问题？"}
        ]),
        None,
    ));
    let users: Vec<_> = out
        .iter()
        .filter(|b| matches!(b, DisplayBlock::User(_)))
        .collect();
    assert_eq!(users.len(), 1, "one band, not a block each: {out:?}");
    match users[0] {
        DisplayBlock::User(u) => {
            // The chip keeps its place ahead of the words it came with.
            assert_eq!(u.text, "[Image #1]\n这张图里排版是不是有问题？");
        }
        _ => unreachable!(),
    }
}

#[test]
fn test_tool_result_images_never_reach_the_outcome_text() {
    let mut p = TranscriptParser::new();
    let payload = "B".repeat(200_000);
    p.push(&tool_use(
        "Read",
        "t1",
        json!({"file_path": "/tmp/shot.png"}),
    ));
    let out = p.push(&row(
        "user",
        json!([{ "type": "tool_result", "tool_use_id": "t1", "content": [
            {"type": "text", "text": "here it is"},
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": payload}}
        ]}]),
        None,
    ));
    let mut blocks = out;
    blocks.extend(p.flush());
    let text = blocks
        .iter()
        .find_map(|b| match b {
            DisplayBlock::Tool(t) => t.result.as_ref().map(|r| r.text.clone()),
            DisplayBlock::ToolGroup(g) => g
                .members
                .iter()
                .find_map(|m| m.result.as_ref().map(|r| r.text.clone())),
            _ => None,
        })
        .expect("a tool outcome");
    assert!(text.contains("here it is"), "{text:.200}");
    assert!(text.contains("[Image #1]"), "{text:.200}");
    assert!(
        !text.contains("BBBB"),
        "payload leaked into the outcome text"
    );
    assert!(text.len() < 1000, "outcome text is {} bytes", text.len());
}

#[test]
fn test_parse_hive_message_reads_every_arrival_shape() {
    // bare: typed straight into the pane.
    let bare = parse_hive_message(
        "<HIVE from=comb.dodo to=comb.rex msgId=a1 reply-to=z9 artifact=/tmp/spec.md>\nreview the spec\n</HIVE>",
    )
    .unwrap();
    assert_eq!(bare.from.as_deref(), Some("comb.dodo"));
    assert_eq!(bare.msg_id.as_deref(), Some("a1"));
    assert_eq!(bare.reply_to.as_deref(), Some("z9"));
    assert_eq!(bare.artifact.as_deref(), Some("/tmp/spec.md"));
    assert_eq!(bare.body, "review the spec");
    assert!(!bare.injected && !bare.mid_turn);

    // claude's session-inbox injection, turn start.
    let turn_start = parse_hive_message(
        "Another Claude session sent a message:\n\
         <HIVE from=sage to=orch msgId=7boK>\ndone\n</HIVE>\n\n\
         This came from another Claude session — not typed by your user, but very \
         likely working on their behalf. …permission laundering.",
    )
    .unwrap();
    assert_eq!(turn_start.from.as_deref(), Some("sage"));
    assert_eq!(turn_start.body, "done");
    assert!(turn_start.injected && !turn_start.mid_turn);

    // same wrapper, folded into a turn already in flight.
    let mid = parse_hive_message(
        "Another Claude session sent a message while you were working:\n\
         <HIVE from=sage to=orch>\ndone\n</HIVE>\n\n\
         This came from another Claude session — …",
    )
    .unwrap();
    assert!(mid.injected && mid.mid_turn);

    // the retired <channel> transport, still in old transcripts.
    let chan = parse_hive_message(
        "<channel source=\"plugin:hive-channel:hive-channel\" msg_id=\"18kd\">\n\
         <HIVE from=validator to=worker msgId=18kd>\nrt-1785757288\n</HIVE>\n</channel>",
    )
    .unwrap();
    assert_eq!(chan.from.as_deref(), Some("validator"));
    assert_eq!(chan.body, "rt-1785757288");

    // attribute-less envelope still parses; the body is what matters.
    let bald = parse_hive_message("<HIVE>hi</HIVE>").unwrap();
    assert_eq!(bald.from, None);
    assert_eq!(bald.body, "hi");
}

#[test]
fn test_every_hive_sender_draws_a_different_agent_icon() {
    let mut p = TranscriptParser::new();
    let mut icons = Vec::new();
    for sender in [
        "sage",
        "scout",
        "dodo",
        "rex",
        "validator",
        "worker",
        "probe",
    ] {
        let out = p.push(&row(
            "user",
            json!(format!("<HIVE from={sender} to=orch>hi</HIVE>")),
            None,
        ));
        match &out[0] {
            DisplayBlock::User(u) => icons.push(u.hive.as_ref().unwrap().icon.unwrap()),
            other => panic!("expected User, got {other:?}"),
        }
    }
    let mut unique = icons.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), AGENT_ICONS.len(), "{icons:?} collided");
    // and the same sender keeps its icon.
    let out = p.push(&row(
        "user",
        json!("<HIVE from=sage to=orch>again</HIVE>"),
        None,
    ));
    match &out[0] {
        DisplayBlock::User(u) => {
            assert_eq!(u.hive.as_ref().unwrap().icon.unwrap(), icons[0]);
        }
        other => panic!("expected User, got {other:?}"),
    }
}

#[test]
fn test_ultra_effort_marker_overrides_the_row_effort() {
    let mut p = TranscriptParser::new();
    p.push(
        &json!({
            "type": "assistant",
            "effort": "xhigh",
            "message": {"role": "assistant", "content": [{"type": "text", "text": "hi"}]},
        })
        .to_string(),
    );
    assert_eq!(p.effort(), Some("xhigh"));
    p.push(
        &json!({"type": "attachment", "attachment": {"type": "ultra_effort_enter", "reminderType": "full"}})
            .to_string(),
    );
    assert_eq!(p.effort(), Some("ultra"));
}

#[test]
fn test_mid_turn_queued_command_is_the_fifth_carrier() {
    let mut p = TranscriptParser::new();
    // A peer envelope absorbed into a running turn: claude records it
    // only as this attachment — no user row ever follows.
    let out = p.push(
        &json!({
            "type": "attachment",
            "timestamp": "2026-08-30T13:00:12.311Z",
            "attachment": {
                "type": "queued_command",
                "prompt": "<HIVE from=scout to=orch msgId=b255>\nscout 报到\n</HIVE>",
                "origin": {"kind": "peer", "from": "honey.orch"},
                "isMeta": true,
            },
        })
        .to_string(),
    );
    match &out[0] {
        DisplayBlock::User(u) => {
            let h = u.hive.as_ref().expect("peer envelope");
            assert_eq!(h.from.as_deref(), Some("scout"));
            assert_eq!(h.body, "scout 报到");
        }
        other => panic!("expected User, got {other:?}"),
    }
    // The human's own mid-turn message lands the same way.
    let out = p.push(
        &json!({
            "type": "attachment",
            "timestamp": "2026-08-30T13:00:20.000Z",
            "attachment": {
                "type": "queued_command",
                "prompt": "顺便把 badge 挪一下",
                "origin": {"kind": "human"},
            },
        })
        .to_string(),
    );
    assert!(matches!(&out[0], DisplayBlock::User(u) if u.hive.is_none()));
    // …and when it also shows up as a user row, it draws only once.
    let again = p.push(&row("user", json!("顺便把 badge 挪一下"), None));
    assert!(
        !again.iter().any(|b| matches!(b, DisplayBlock::User(_))),
        "{again:?}"
    );
    // Runtime plumbing carries no origin and stays out.
    let noise = p.push(
        &json!({
            "type": "attachment",
            "attachment": {
                "type": "queued_command",
                "prompt": "<task-notification>\n<task-id>abc</task-id>",
            },
        })
        .to_string(),
    );
    assert!(noise.is_empty(), "{noise:?}");
}

#[test]
fn test_absorbed_queue_row_draws_when_no_attachment_carried_it() {
    let envelope = "<HIVE from=sage to=orch msgId=9nMW>\nattach 后投递正常\n</HIVE>";
    let absorbed = json!({
        "type": "queue-operation",
        "operation": "remove",
        "reason": "absorbed_mid_turn",
        "timestamp": "2026-08-30T13:00:19.267Z",
        "content": envelope,
    })
    .to_string();

    // No attachment row: the terminal state is the only record left.
    let mut p = TranscriptParser::new();
    let out = p.push(&absorbed);
    assert!(
        matches!(&out[0], DisplayBlock::User(u) if u.hive.as_ref().unwrap().from.as_deref() == Some("sage")),
        "{out:?}"
    );

    // With the attachment row, the terminal state adds nothing.
    let mut p = TranscriptParser::new();
    let first = p.push(
        &json!({
            "type": "attachment",
            "attachment": {
                "type": "queued_command",
                "prompt": envelope,
                "origin": {"kind": "peer", "from": "honey.orch"},
            },
        })
        .to_string(),
    );
    assert_eq!(first.len(), 1);
    assert!(p.push(&absorbed).is_empty());

    // enqueue is not a terminal state — it may still be cancelled.
    let mut p = TranscriptParser::new();
    assert!(p
        .push(
            &json!({"type": "queue-operation", "operation": "enqueue", "content": envelope})
                .to_string()
        )
        .is_empty());
}

#[test]
fn test_parse_hive_message_ignores_prose_that_quotes_an_envelope() {
    // skill docs and specs quote the envelope; they are not messages.
    assert!(parse_hive_message(
        "其他 agent 的消息会以 `<HIVE from=a to=b>body</HIVE>` 注入当前 pane。"
    )
    .is_none());
    assert!(parse_hive_message("<HIVE from=a to=b>unterminated").is_none());
    assert!(parse_hive_message("<HIVEISH from=a>x</HIVE>").is_none());
    // a body that merely mentions the tag stays whole.
    let msg =
        parse_hive_message("<HIVE from=probe to=kilo>你上下文里的 <HIVE> 消息</HIVE>").unwrap();
    assert_eq!(msg.body, "你上下文里的 <HIVE> 消息");
}

#[test]
fn test_hive_envelope_collapses_to_a_tagged_line() {
    let mut p = StreamPrinter::new();
    let body = "<HIVE from=comb.dodo to=comb.rex msgId=a1>review the spec</HIVE>";
    let out = p.push_rendered(&row("user", json!(body), None)).unwrap();
    assert!(out.contains("✉") && out.contains("comb.dodo") && out.contains("review the spec"));
    assert!(!out.contains("<HIVE"));
    assert!(p.working);
}

#[test]
fn test_user_turn_flips_working_and_final_text_flips_idle() {
    let mut p = StreamPrinter::new();
    p.push_rendered(&row("user", json!("hi"), None));
    assert!(p.working);
    p.push_rendered(&text("assistant", "hello"));
    assert!(!p.working);
}

#[test]
fn test_output_tokens_accumulate_into_the_status_line() {
    let mut p = StreamPrinter::new();
    p.push_rendered(&row(
        "assistant",
        json!([{"type": "text", "text": "a"}]),
        Some(json!({"output_tokens": 40})),
    ));
    p.push_rendered(&row(
        "assistant",
        json!([{"type": "text", "text": "b"}]),
        Some(json!({"output_tokens": 2})),
    ));
    assert!(p.status_line(0, "deadbeef-1234").contains("42 tokens out"));
    assert_eq!(p.parser.output_tokens(), 42);
}

#[test]
fn test_non_message_rows_render_nothing() {
    let mut p = StreamPrinter::new();
    assert!(p
        .push_rendered(&json!({"type": "system"}).to_string())
        .is_none());
    assert!(p.push_rendered("not json").is_none());
    assert!(p.parser.pending_blocks().is_empty());
}

// ---- parser: tool-group aggregation --------------------------------

fn group(block: &DisplayBlock) -> &ToolGroupBlock {
    match block {
        DisplayBlock::ToolGroup(g) => g,
        other => panic!("expected ToolGroup, got {other:?}"),
    }
}

#[test]
fn test_consecutive_read_tools_collapse_into_one_group() {
    let mut p = TranscriptParser::new();
    assert!(p
        .push(&tool_use("Read", "t1", json!({"file_path": "/a.rs"})))
        .is_empty());
    assert!(p
        .push(&tool_use("Grep", "t2", json!({"pattern": "fn main"})))
        .is_empty());
    let out = p.push(&text("assistant", "done"));
    assert_eq!(out.len(), 2, "{out:?}");
    assert_eq!(group(&out[0]).label(), "Read 1 file, Searched 1 pattern");
    assert!(matches!(out[1], DisplayBlock::Assistant(_)));
}

#[test]
fn test_group_label_pluralizes_bucket_counts() {
    let mut p = TranscriptParser::new();
    p.push(&tool_use("Read", "t1", json!({"file_path": "/a.rs"})));
    p.push(&tool_use("Read", "t2", json!({"file_path": "/b.rs"})));
    p.push(&tool_use("Glob", "t3", json!({"pattern": "*.rs"})));
    let out = p.flush();
    assert_eq!(group(&out[0]).label(), "Read 2 files, Searched 1 pattern");
}

#[test]
fn test_group_bucket_order_follows_first_appearance() {
    let mut p = TranscriptParser::new();
    p.push(&tool_use("Grep", "t1", json!({"pattern": "x"})));
    p.push(&tool_use("Read", "t2", json!({"file_path": "/a.rs"})));
    let out = p.flush();
    assert_eq!(group(&out[0]).label(), "Searched 1 pattern, Read 1 file");
}

#[test]
fn test_group_closes_when_a_bash_tool_arrives() {
    let mut p = TranscriptParser::new();
    p.push(&tool_use("Read", "t1", json!({"file_path": "/a.rs"})));
    let out = p.push(&row(
        "assistant",
        json!([{"type": "tool_use", "id": "t2", "name": "Bash",
                "input": {"command": "cargo build"}}]),
        None,
    ));
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(group(&out[0]).label(), "Read 1 file");
    let pending = p.pending_blocks();
    assert!(
        matches!(&pending[..], [DisplayBlock::Run(_)]),
        "{pending:?}"
    );
}

#[test]
fn test_tool_results_do_not_break_an_open_group() {
    let mut p = TranscriptParser::new();
    p.push(&tool_use("Read", "t1", json!({"file_path": "/a.rs"})));
    assert!(p
        .push(&tool_result("t1", json!("1\tfn a"), false))
        .is_empty());
    p.push(&tool_use("Read", "t2", json!({"file_path": "/b.rs"})));
    let out = p.push(&text("assistant", "done"));
    let g = group(&out[0]);
    assert_eq!(g.members.len(), 2);
    assert_eq!(g.label(), "Read 2 files");
    assert_eq!(
        g.members[0].result.as_ref().unwrap().first_line(),
        "1\tfn a"
    );
}

#[test]
fn test_group_counts_failed_members() {
    let mut p = TranscriptParser::new();
    p.push(&tool_use("Read", "t1", json!({"file_path": "/a.rs"})));
    p.push(&tool_use("Read", "t2", json!({"file_path": "/gone.rs"})));
    p.push(&tool_result("t2", json!("no such file"), true));
    let out = p.flush();
    let g = group(&out[0]);
    assert_eq!(g.failed(), 1);
    assert!(g.members[1].result.as_ref().unwrap().is_error);
}

#[test]
fn test_skill_read_buckets_as_skill() {
    let mut p = TranscriptParser::new();
    p.push(&tool_use(
        "Read",
        "t1",
        json!({"file_path": "/plugins/x/skills/y/SKILL.md"}),
    ));
    let out = p.flush();
    assert_eq!(group(&out[0]).label(), "Read 1 skill");
}

// ---- parser: bash run wording --------------------------------------

fn run(block: &DisplayBlock) -> &RunBlock {
    match block {
        DisplayBlock::Run(r) => r,
        other => panic!("expected Run, got {other:?}"),
    }
}

#[test]
fn test_bash_description_strips_run_prefix_and_newlines() {
    let mut p = TranscriptParser::new();
    p.push(&tool_use(
        "Bash",
        "t1",
        json!({"command": "cargo test", "description": "Run the\ntests"}),
    ));
    let out = p.flush();
    assert_eq!(run(&out[0]).description, "the tests");
}

#[test]
fn test_bash_falls_back_to_command_first_line() {
    let mut p = TranscriptParser::new();
    p.push(&tool_use("Bash", "t1", json!({"command": "ls -la\npwd"})));
    p.push(&tool_use("Bash", "t2", json!({})));
    let out = p.flush();
    assert_eq!(run(&out[0]).description, "ls -la");
    assert_eq!(run(&out[1]).description, "…");
}

#[test]
fn test_run_finalizes_when_its_result_attaches() {
    let mut p = TranscriptParser::new();
    assert!(p
        .push(&tool_use("Bash", "t1", json!({"command": "cargo build"})))
        .is_empty());
    let out = p.push(&tool_result("t1", json!("Compiling hive"), false));
    assert_eq!(out.len(), 1, "{out:?}");
    let r = run(&out[0]);
    assert_eq!(r.result.as_ref().unwrap().first_line(), "Compiling hive");
    assert!(p.pending_blocks().is_empty());
}

#[test]
fn test_other_tools_keep_name_and_hint() {
    let mut p = TranscriptParser::new();
    p.push(&tool_use("Edit", "t1", json!({"file_path": "/a.rs"})));
    let out = p.flush();
    match &out[0] {
        DisplayBlock::Tool(t) => {
            assert_eq!(t.name, "Edit");
            assert_eq!(t.hint, "/a.rs");
        }
        other => panic!("expected Tool, got {other:?}"),
    }
}

// ---- parser: thinking ----------------------------------------------

#[test]
fn test_thinking_duration_comes_from_adjacent_row_timestamps() {
    let mut p = TranscriptParser::new();
    p.push(&row_at(
        "user",
        json!("go"),
        None,
        Some("2026-08-30T12:40:00.000Z"),
    ));
    let out = p.push(&row_at(
        "assistant",
        json!([{"type": "thinking", "thinking": "hmm"}]),
        None,
        Some("2026-08-30T12:40:14.300Z"),
    ));
    match &out[0] {
        DisplayBlock::Thinking(t) => {
            assert_eq!(t.duration_secs, Some(14.3));
            assert_eq!(t.label(), "Thought for 14.3s");
        }
        other => panic!("expected Thinking, got {other:?}"),
    }
}

#[test]
fn test_thinking_without_timestamps_has_no_duration() {
    let mut p = TranscriptParser::new();
    let out = p.push(&row(
        "assistant",
        json!([{"type": "thinking", "thinking": "hmm"}]),
        None,
    ));
    match &out[0] {
        DisplayBlock::Thinking(t) => {
            assert_eq!(t.duration_secs, None);
            assert_eq!(t.label(), "Thought");
        }
        other => panic!("expected Thinking, got {other:?}"),
    }
}

#[test]
fn test_thinking_breaks_an_open_tool_group() {
    let mut p = TranscriptParser::new();
    p.push(&tool_use("Read", "t1", json!({"file_path": "/a.rs"})));
    let out = p.push(&row(
        "assistant",
        json!([{"type": "thinking", "thinking": "hmm"}]),
        None,
    ));
    assert_eq!(out.len(), 2, "{out:?}");
    assert_eq!(group(&out[0]).label(), "Read 1 file");
    assert!(matches!(out[1], DisplayBlock::Thinking(_)));
}

// ---- parser: worked-for --------------------------------------------

#[test]
fn test_worked_for_spans_user_msg_to_final_assistant_text() {
    let mut p = TranscriptParser::new();
    p.push(&row_at(
        "user",
        json!("go"),
        None,
        Some("2026-08-30T12:40:00.000Z"),
    ));
    p.push(&row_at(
        "assistant",
        json!([{"type": "text", "text": "done"}]),
        None,
        Some("2026-08-30T12:44:06.000Z"),
    ));
    let out = p.push(&row_at(
        "user",
        json!("next"),
        None,
        Some("2026-08-30T12:50:00.000Z"),
    ));
    assert_eq!(out.len(), 2, "{out:?}");
    match &out[0] {
        DisplayBlock::WorkedFor(w) => {
            assert_eq!(w.duration_secs, Some(246.0));
            assert_eq!(w.label(), "Worked for 4m6s");
        }
        other => panic!("expected WorkedFor, got {other:?}"),
    }
    assert!(matches!(out[1], DisplayBlock::User(_)));
}

#[test]
fn test_worked_for_skipped_without_assistant_text() {
    let mut p = TranscriptParser::new();
    p.push(&row("user", json!("go"), None));
    let out = p.push(&row("user", json!("actually wait"), None));
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(matches!(out[0], DisplayBlock::User(_)));
}

// ---- durations & timestamps ----------------------------------------

#[test]
fn test_thinking_duration_formats() {
    assert_eq!(format_thinking_duration(0.84), "0.8s");
    assert_eq!(format_thinking_duration(14.3), "14.3s");
    assert_eq!(format_thinking_duration(72.4), "1m12s");
}

#[test]
fn test_worked_for_duration_formats() {
    assert_eq!(format_worked_duration(4.42), "4.4s");
    assert_eq!(format_worked_duration(32.9), "32s");
    assert_eq!(format_worked_duration(246.0), "4m6s");
    assert_eq!(format_worked_duration(3720.0), "1h2m");
}

#[test]
fn test_timestamps_render_local_clock() {
    let mut env = crate::testenv::EnvGuard::new();
    env.set("TZ", "UTC");
    let epoch = parse_timestamp("1970-01-01T00:00:00.000Z").unwrap();
    assert_eq!(epoch.epoch_ms, 0);
    assert_eq!(epoch.clock, "12:00 AM");
    let noonish = parse_timestamp("2026-08-30T12:40:03.500Z").unwrap();
    assert_eq!(noonish.clock, "12:40 PM");
    let morning = parse_timestamp("2026-08-30T09:05:00Z").unwrap();
    assert_eq!(morning.clock, "9:05 AM");
}

#[test]
fn test_user_and_assistant_blocks_carry_timestamps() {
    let mut env = crate::testenv::EnvGuard::new();
    env.set("TZ", "UTC");
    let mut p = TranscriptParser::new();
    let out = p.push(&row_at(
        "user",
        json!("hello"),
        None,
        Some("2026-08-30T12:40:00.000Z"),
    ));
    match &out[0] {
        DisplayBlock::User(u) => {
            assert!(u.hive.is_none());
            assert_eq!(u.timestamp.as_ref().unwrap().clock, "12:40 PM");
        }
        other => panic!("expected User, got {other:?}"),
    }
    let out = p.push(&row_at(
        "assistant",
        json!([{"type": "text", "text": "hi"}]),
        None,
        Some("2026-08-30T12:41:00.000Z"),
    ));
    match &out[0] {
        DisplayBlock::Assistant(a) => {
            assert_eq!(a.markdown, "hi");
            assert_eq!(a.timestamp.as_ref().unwrap().clock, "12:41 PM");
        }
        other => panic!("expected Assistant, got {other:?}"),
    }
}

#[test]
fn test_pending_blocks_snapshot_shows_open_group() {
    let mut p = TranscriptParser::new();
    p.push(&tool_use("Read", "t1", json!({"file_path": "/a.rs"})));
    p.push(&tool_use("Read", "t2", json!({"file_path": "/b.rs"})));
    let pending = p.pending_blocks();
    assert_eq!(pending.len(), 1, "{pending:?}");
    let g = group(&pending[0]);
    assert_eq!(g.members.len(), 2);
    assert_eq!(g.label(), "Read 2 files");
    assert!(p.busy());
}

// ---- parser: full-content capture ----------------------------------

#[test]
fn test_thinking_block_captures_full_text() {
    let mut p = TranscriptParser::new();
    let out = p.push(&row(
        "assistant",
        json!([{"type": "thinking", "thinking": "deep\nthought\nhere"}]),
        None,
    ));
    match &out[0] {
        DisplayBlock::Thinking(t) => assert_eq!(t.text, "deep\nthought\nhere"),
        other => panic!("expected Thinking, got {other:?}"),
    }
}

#[test]
fn test_tool_outcome_stores_full_text_and_derives_first_line() {
    let mut p = TranscriptParser::new();
    p.push(&tool_use("Bash", "t1", json!({"command": "cargo build"})));
    let out = p.push(&tool_result(
        "t1",
        json!("Compiling hive\nFinished dev\nwarning: unused"),
        false,
    ));
    let res = run(&out[0]).result.as_ref().unwrap().clone();
    assert_eq!(res.text, "Compiling hive\nFinished dev\nwarning: unused");
    assert!(!res.truncated);
    assert_eq!(res.first_line(), "Compiling hive");
}

#[test]
fn test_first_line_clips_long_lines_with_ellipsis() {
    let res = ToolOutcome::new("a".repeat(200), false);
    assert_eq!(res.first_line(), format!("{} …", "a".repeat(160)));
    assert!(!res.truncated, "200 chars is far below the storage cap");
    assert_eq!(res.text.len(), 200, "storage keeps the full text");
}

#[test]
fn test_tool_outcome_truncates_at_byte_cap() {
    let res = ToolOutcome::new("x".repeat(TOOL_RESULT_MAX_BYTES + 1000), true);
    assert!(res.truncated);
    assert!(res.is_error);
    assert_eq!(res.text.len(), TOOL_RESULT_MAX_BYTES);
    let ok = ToolOutcome::new("x".repeat(TOOL_RESULT_MAX_BYTES), false);
    assert!(!ok.truncated, "exactly at the cap is not truncated");
    assert_eq!(ok.text.len(), TOOL_RESULT_MAX_BYTES);
}

#[test]
fn test_tool_outcome_truncation_respects_char_boundaries() {
    // '宽' is 3 bytes; the cap is not a multiple of 3, so a naive byte
    // cut would split a char.
    let count = TOOL_RESULT_MAX_BYTES / 3 + 100;
    let res = ToolOutcome::new("宽".repeat(count), false);
    assert!(res.truncated);
    assert!(res.text.len() <= TOOL_RESULT_MAX_BYTES);
    assert!(res.text.chars().all(|c| c == '宽'), "no split chars");
}

#[test]
fn test_run_block_keeps_full_command() {
    let mut p = TranscriptParser::new();
    p.push(&tool_use(
        "Bash",
        "t1",
        json!({"command": "ls -la\npwd", "description": "List files"}),
    ));
    p.push(&tool_use("Bash", "t2", json!({})));
    let out = p.flush();
    assert_eq!(run(&out[0]).command, "ls -la\npwd");
    assert_eq!(run(&out[1]).command, "", "absent command stores empty");
}

#[test]
fn test_tool_block_keeps_full_input_json() {
    let input = json!({"file_path": "/a.rs", "old_string": "line1\nline2",
                       "new_string": "line3"});
    let mut p = TranscriptParser::new();
    p.push(&tool_use("Edit", "t1", input.clone()));
    let out = p.flush();
    match &out[0] {
        DisplayBlock::Tool(t) => {
            let parsed: Value = serde_json::from_str(&t.input_json).unwrap();
            assert_eq!(parsed, input, "{}", t.input_json);
        }
        other => panic!("expected Tool, got {other:?}"),
    }
}

#[test]
fn test_group_member_keeps_full_input_json() {
    let input = json!({"pattern": "fn main", "path": "/src", "-n": true});
    let mut p = TranscriptParser::new();
    p.push(&tool_use("Grep", "t1", input.clone()));
    let out = p.flush();
    let g = group(&out[0]);
    let parsed: Value = serde_json::from_str(&g.members[0].input_json).unwrap();
    assert_eq!(parsed, input);
}

// ---- parser: entry identity & turns --------------------------------

#[test]
fn test_entry_ids_stable_from_pending_through_finalization() {
    let mut p = TranscriptParser::new();
    assert!(p
        .push_entries(&tool_use("Read", "t1", json!({"file_path": "/a.rs"})))
        .is_empty());
    let snap = p.pending_entries();
    assert_eq!(snap.len(), 1);
    let group_id = snap[0].id;
    // Aggregating another member keeps the group's id.
    p.push_entries(&tool_use("Read", "t2", json!({"file_path": "/b.rs"})));
    assert_eq!(p.pending_entries()[0].id, group_id);
    // Finalization emits the same id, and later blocks mint higher ones.
    let out = p.push_entries(&text("assistant", "done"));
    assert_eq!(out.len(), 2, "{out:?}");
    assert_eq!(out[0].id, group_id);
    assert!(matches!(out[0].block, DisplayBlock::ToolGroup(_)));
    assert!(out[1].id > group_id);
    assert!(matches!(out[1].block, DisplayBlock::Assistant(_)));
}

#[test]
fn test_entry_ids_never_collide_between_pending_and_finalized() {
    let mut p = TranscriptParser::new();
    p.push_entries(&tool_use("Read", "t1", json!({"file_path": "/a.rs"})));
    let finalized = p.push_entries(&row(
        "assistant",
        json!([{"type": "thinking", "thinking": "hmm"}]),
        None,
    ));
    p.push_entries(&tool_use("Bash", "t2", json!({"command": "ls"})));
    let pending = p.pending_entries();
    let mut ids: Vec<u64> = finalized
        .iter()
        .chain(pending.iter())
        .map(|e| e.id)
        .collect();
    assert_eq!(
        ids,
        {
            let mut sorted = ids.clone();
            sorted.sort_unstable();
            sorted
        },
        "ids are monotonic in display order"
    );
    ids.dedup();
    assert_eq!(ids.len(), 3, "group, thinking, run all distinct: {ids:?}");
    // The pending run finalizes with the id its snapshot showed.
    let out = p.flush_entries();
    assert_eq!(out[0].id, pending[0].id);
}

#[test]
fn test_user_blocks_start_turns() {
    let mut p = TranscriptParser::new();
    let out = p.push_entries(&row_at(
        "user",
        json!("go"),
        None,
        Some("2026-08-30T12:40:00.000Z"),
    ));
    assert!(out[0].block.starts_turn());
    let out = p.push_entries(&row_at(
        "assistant",
        json!([{"type": "text", "text": "done"}]),
        None,
        Some("2026-08-30T12:41:00.000Z"),
    ));
    assert!(!out[0].block.starts_turn());
    // WorkedFor emitted ahead of the next user prompt is not a turn start.
    let out = p.push_entries(&row_at(
        "user",
        json!("next"),
        None,
        Some("2026-08-30T12:42:00.000Z"),
    ));
    assert!(matches!(out[0].block, DisplayBlock::WorkedFor(_)));
    assert!(!out[0].block.starts_turn());
    assert!(out[1].block.starts_turn());
}

// ---- line accumulator: partial rows across reads ---------------------

#[test]
fn test_line_accumulator_row_written_in_two_chunks_parses_once() {
    let row = format!("{}\n", text("assistant", "two chunks"));
    let (head, tail) = row.as_bytes().split_at(row.len() / 2);
    let mut lines = LineAccumulator::new();
    let mut p = TranscriptParser::new();
    assert_eq!(lines.push(head), None);
    let whole = lines.push(tail).expect("row completes on the newline");
    let entries = p.push_entries(&whole);
    assert_eq!(entries.len(), 1, "{whole}");
    assert_eq!(whole, row.trim_end());
}

#[test]
fn test_line_accumulator_row_cut_inside_a_multibyte_char_parses_once() {
    let row = format!("{}\n", text("assistant", "你好世界"));
    let cut = row.find('好').unwrap() + 1;
    assert!(!row.is_char_boundary(cut));
    let (head, tail) = row.as_bytes().split_at(cut);
    let mut lines = LineAccumulator::new();
    assert_eq!(lines.push(head), None);
    let whole = lines.push(tail).expect("row completes on the newline");
    assert_eq!(whole, row.trim_end());
    let mut p = TranscriptParser::new();
    let entries = p.push_entries(&whole);
    assert_eq!(entries.len(), 1, "{whole}");
    match &entries[0].block {
        DisplayBlock::Assistant(a) => assert_eq!(a.markdown, "你好世界"),
        other => panic!("expected Assistant, got {other:?}"),
    }
}

#[test]
fn test_line_accumulator_backlog_holds_trailing_partial_row() {
    let first = text("user", "first");
    let second = text("assistant", "second");
    let cut = second.len() / 2;
    let backlog = format!("{first}\n{}", &second[..cut]);
    let mut lines = LineAccumulator::new();
    let whole = lines.split_backlog(backlog.as_bytes());
    assert_eq!(whole, vec![first.clone()]);
    let mut p = TranscriptParser::new();
    for raw in &whole {
        p.push_entries(raw);
    }
    let rest = format!("{}\n", &second[cut..]);
    let completed = lines
        .push(rest.as_bytes())
        .expect("remainder completes the row");
    assert_eq!(completed, second);
    assert_eq!(p.push_entries(&completed).len(), 1);
}

#[test]
fn test_line_accumulator_backlog_holds_partial_row_cut_inside_a_char() {
    let second = text("assistant", "尾行");
    let cut = second.find('尾').unwrap() + 2;
    assert!(!second.is_char_boundary(cut));
    let mut backlog = format!("{}\n", text("user", "head")).into_bytes();
    backlog.extend_from_slice(&second.as_bytes()[..cut]);
    let mut lines = LineAccumulator::new();
    let whole = lines.split_backlog(&backlog);
    assert_eq!(whole.len(), 1);
    let mut rest = second.as_bytes()[cut..].to_vec();
    rest.push(b'\n');
    assert_eq!(lines.push(&rest).as_deref(), Some(second.as_str()));
}

#[test]
fn test_line_accumulator_complete_row_emits_immediately() {
    let mut lines = LineAccumulator::new();
    let row = text("assistant", "whole");
    assert_eq!(
        lines.push(format!("{row}\n").as_bytes()).as_deref(),
        Some(row.as_str())
    );
    assert_eq!(
        lines.push(format!("{row}\r\n").as_bytes()).as_deref(),
        Some(row.as_str())
    );
    let mut p = StreamPrinter::new();
    assert!(p.push_rendered(&row).unwrap().contains("whole"));
}
