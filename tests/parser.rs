use orch::{
    codex::Parser,
    events::{AgentEvent, FailureKind},
};

#[test]
fn captured_turns_keep_id_and_do_not_duplicate_messages() {
    for (fixture, text) in [
        (
            include_bytes!("fixtures/codex-0.172.0/turn-1.jsonl").as_slice(),
            "OK",
        ),
        (
            include_bytes!("fixtures/codex-0.172.0/turn-2.jsonl").as_slice(),
            "ORCH-1A-7F3C",
        ),
    ] {
        let mut parser = Parser::default();
        let mut messages = 0;
        for line in fixture
            .split(|b| *b == b'\n')
            .filter(|line| !line.is_empty())
        {
            for event in parser.line(line) {
                if matches!(event, AgentEvent::MessageCompleted { .. }) {
                    messages += 1;
                }
                assert!(!matches!(event, AgentEvent::TurnCompleted { .. }));
            }
        }
        let result = parser
            .finish(Some("00000000-0000-4000-8000-000000000001"))
            .expect("captured turn");
        assert_eq!(result.final_text, text);
        assert_eq!(messages, 1);
    }
}

#[test]
fn invalid_unknown_incomplete_and_provider_errors_are_not_success() {
    let mut parser = Parser::default();
    assert!(parser.line(b"not json").is_empty());
    assert!(parser.line(b"\xff").is_empty());
    assert!(
        parser
            .line(br#"{"type":"future.event","extra":1}"#)
            .is_empty()
    );
    assert!(parser.finish(None).is_err());
    parser.line(br#"{"type":"thread.started","thread_id":"thread"}"#);
    parser.line(br#"{"type":"turn.started"}"#);
    let message =
        br#"{"type":"item.completed","item":{"type":"agent_message","id":"i","text":"partial"}}"#;
    assert_eq!(parser.line(message).len(), 1);
    assert!(parser.line(message).is_empty());
    assert!(parser.finish(None).is_err());
    parser.line(br#"{"type":"turn.failed","error":{"message":"synthetic failure"}}"#);
    let (kind, message) = parser.finish(None).expect_err("failed turn");
    assert_eq!(kind, FailureKind::Provider);
    assert_eq!(message, "synthetic failure");
}

#[test]
fn malformed_known_events_and_duplicate_completion_fail_closed() {
    for suffix in [
        br#"{"type":"turn.completed"}"#.as_slice(),
        br#"{"type":"thread.started"}"#.as_slice(),
    ] {
        let mut parser = Parser::default();
        for line in include_bytes!("fixtures/codex-0.172.0/turn-1.jsonl")
            .split(|b| *b == b'\n')
            .filter(|line| !line.is_empty())
        {
            parser.line(line);
        }
        parser.line(suffix);
        assert_eq!(
            parser.finish(None).expect_err("invalid sequence").0,
            FailureKind::Protocol
        );
    }
}

fn started_parser() -> Parser {
    let mut parser = Parser::default();
    parser.line(br#"{"type":"thread.started","thread_id":"thread"}"#);
    parser.line(br#"{"type":"turn.started"}"#);
    parser
}

#[test]
fn malformed_or_out_of_order_events_cannot_be_repaired_into_success() {
    let cases: &[&[&[u8]]] = &[
        &[br#"{"type":"thread.started"}"#],
        &[br#"{"type":"thread.started","thread_id":""}"#],
        &[br#"{"type":"turn.started"}"#],
        &[
            br#"{"type":"thread.started","thread_id":"thread"}"#,
            br#"{"type":"thread.started","thread_id":"thread"}"#,
        ],
        &[
            br#"{"type":"thread.started","thread_id":"thread"}"#,
            br#"{"type":"turn.started"}"#,
            br#"{"type":"turn.started"}"#,
        ],
        &[br#"{"type":"item.completed","item":{"type":"agent_message","id":"i","text":"early"}}"#],
    ];
    for lines in cases {
        let mut parser = Parser::default();
        for line in *lines {
            parser.line(line);
        }
        // Later well-formed events must not hide the first protocol failure.
        parser.line(br#"{"type":"thread.started","thread_id":"thread"}"#);
        parser.line(br#"{"type":"turn.started"}"#);
        parser.line(
            br#"{"type":"item.completed","item":{"type":"agent_message","id":"i","text":"late"}}"#,
        );
        parser.line(br#"{"type":"turn.completed"}"#);
        assert_eq!(
            parser.finish(None).expect_err("invalid sequence").0,
            FailureKind::Protocol
        );
    }
    let mut parser = started_parser();
    parser.line(br#"{"type":"item.completed","item":{"type":"agent_message","id":"i"}}"#);
    assert_eq!(
        parser.finish(None).expect_err("missing text").0,
        FailureKind::Protocol
    );
    for (line, message) in [
        (
            br#"{"type":"error","message":"synthetic transport error"}"#.as_slice(),
            "synthetic transport error",
        ),
        (
            br#"{"type":"error"}"#.as_slice(),
            "provider reported an error",
        ),
    ] {
        let mut parser = Parser::default();
        parser.line(line);
        let failure = parser.finish(None).expect_err("provider error before init");
        assert_eq!(failure, (FailureKind::Provider, message.to_owned()));
    }
}

#[test]
fn message_replacements_preserve_order_without_duplicating_content() {
    let mut parser = started_parser();
    for (id, text) in [
        ("first", "draft"),
        ("second", "second message"),
        ("first", "corrected"),
    ] {
        let event = serde_json::json!({"type":"item.completed","item":{"type":"agent_message","id":id,"text":text}});
        assert_eq!(parser.line(event.to_string().as_bytes()).len(), 1);
    }
    parser.line(br#"{"type":"turn.completed"}"#);
    assert_eq!(
        parser.finish(Some("thread")).expect("turn").final_text,
        "corrected\n\nsecond message"
    );
    assert_eq!(
        parser
            .finish(Some("another-thread"))
            .expect_err("wrong resume id")
            .0,
        FailureKind::Protocol
    );
}

#[test]
fn accumulated_message_content_and_ids_are_bounded() {
    for oversized_id in [false, true] {
        let mut parser = started_parser();
        for i in 0..5 {
            let (id, text) = if oversized_id {
                (format!("{i}{}", "i".repeat(900_000)), String::new())
            } else {
                (i.to_string(), "x".repeat(900_000))
            };
            let event = serde_json::json!({"type":"item.completed","item":{"type":"agent_message","id":id,"text":text}});
            parser.line(event.to_string().as_bytes());
        }
        parser.line(br#"{"type":"turn.completed"}"#);
        let (kind, message) = parser.finish(None).expect_err("memory ceiling");
        assert_eq!(kind, FailureKind::Protocol);
        assert!(message.contains("memory limit"));
    }
}
