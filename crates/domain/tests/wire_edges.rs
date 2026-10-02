//! De invoergrenzen die niet zichtbaar zijn in alleen gevulde structfixtures.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use spin_domain::{self as d, Wire};

#[test]
fn missing_and_explicit_null_raw_json_remain_distinct() {
    let missing = d::AgentProcess::from_json(b"{}").unwrap();
    let explicit = d::AgentProcess::from_json(br#"{"settings":null}"#).unwrap();
    assert!(!missing.to_json().unwrap().contains("settings"));
    assert!(explicit.to_json().unwrap().contains("\"settings\":null"));
}
#[test]
fn null_does_not_erase_scalar_but_does_erase_pointer() {
    let mut job = d::Job::from_json(br#"{"title":"kept"}"#).unwrap();
    job.merge_value(&d::json::parse(br#"{"title":null}"#).unwrap())
        .unwrap();
    assert_eq!(job.title, "kept");
    let mut composition =
        d::Composition::from_json(br#"{"runtime":{"status":"running"}}"#).unwrap();
    composition
        .merge_value(&d::json::parse(br#"{"runtime":null}"#).unwrap())
        .unwrap();
    assert!(composition.runtime.is_none());
}
#[test]
fn bad_types_and_binary_encodings_are_errors() {
    for json in [
        br#"{"title":17}"#.as_slice(),
        br#"{"brainstorm":"yes"}"#,
        br#"{"repositories":{}}"#,
    ] {
        assert!(d::CreateJobRequest::from_json(json).is_err());
    }
    for invalid in ["a", "====", "a===", "a=aa", "a!aa", "YQ==YQ=="] {
        let json = format!("\"{invalid}\"");
        assert!(d::Bytes::from_json(json.as_bytes()).is_err());
    }
    assert_eq!(
        d::Bytes::from_json(br#""AP8=""#).unwrap().0.unwrap(),
        [0, 255]
    );
}
#[test]
fn runner_chunks_use_the_frame_budget_not_the_jobspec_budget() {
    let mut json = String::from("{\"type\":\"stream_data\",\"data\":\"");
    json.push_str(&"YWFh".repeat((1 << 20) / 3));
    json.push_str("\"}");
    assert!(d::protocol::WireMessage::from_json(json.as_bytes()).is_err());
    let message = d::protocol::WireMessage::decode(json.as_bytes()).unwrap();
    assert!(message.data.0.unwrap().len() > 1_000_000);
    assert!(d::json::parse_with_limit(b"{}", 1).is_err());
}
#[test]
fn wire_version_is_required_for_first_message() {
    let mut hello =
        d::protocol::WireMessage::decode(br#"{"type":"hello","version":1,"instance_id":"host-1"}"#)
            .unwrap();
    assert!(hello.is_supported_hello());
    hello.version = 2;
    assert!(!hello.is_supported_hello());
}
