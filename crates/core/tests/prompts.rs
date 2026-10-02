//! Contrasten worden door de oorspronkelijke Go-server uit zijn echte Store opgebouwd.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use spin_domain::{self as d, Wire};
#[test]
fn workflow_context_matches_the_original_go_prompt_byte_for_byte() {
    let contracts = d::json::Value::from_json(include_bytes!("fixtures/prompts.json")).unwrap();
    for contract in contracts.as_array().unwrap() {
        let item = contract.as_object().unwrap();
        let snapshot = d::Snapshot::from_value(item.get("snapshot").unwrap()).unwrap();
        let job = d::Job::from_value(item.get("job").unwrap()).unwrap();
        let session = d::Session::from_value(item.get("session").unwrap()).unwrap();
        let run = d::PhaseRun::from_value(item.get("run").unwrap()).unwrap();
        let phase = d::WorkflowPhase::from_value(item.get("phase").unwrap()).unwrap();
        let expected = item.get("prompt").unwrap().as_str().unwrap();
        let actual = spin_core::prompts::workflow(&snapshot, &job, &session, &run, &phase).unwrap();
        assert_eq!(actual, expected, "phase {}", phase.id);
    }
}
