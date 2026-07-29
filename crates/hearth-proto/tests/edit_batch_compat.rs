//! Wire-compatibility contract for the batch-edit types: a Hearth 0.1.0
//! caller that has never heard of the new opt-in fields must round-trip
//! unchanged, and responses must not grow fields nobody asked for.

use hearth_proto::{EditBatchParams, EditBatchResult, WhitespaceOnlyTargetPolicy};

/// The exact parameter JSON a pre-policy client sends.
const LEGACY_PARAMS: &str = r#"{
    "path": "/tmp/a.txt",
    "edits": [{ "oldText": "a", "newText": "b" }],
    "diffContext": 4,
    "skipDiff": true,
    "returnContent": true,
    "mode": "inPlace",
    "followSymlinks": true
}"#;

#[test]
fn legacy_params_default_to_hearth_010_behavior() {
    let params: EditBatchParams = serde_json::from_str(LEGACY_PARAMS).unwrap();
    assert!(!params.return_original_content);
    assert_eq!(
        params.whitespace_only_target_policy,
        WhitespaceOnlyTargetPolicy::Reject
    );
}

#[test]
fn new_params_round_trip_through_named_msgpack() {
    let mut params = EditBatchParams::new(
        "/tmp/a.txt",
        vec![hearth_proto::EditReplacement {
            old_text: "a".into(),
            new_text: "b".into(),
        }],
    );
    params.return_original_content = true;
    params.whitespace_only_target_policy = WhitespaceOnlyTargetPolicy::ExactFile;

    let bytes = rmp_serde::to_vec_named(&params).unwrap();
    let back: EditBatchParams = rmp_serde::from_slice(&bytes).unwrap();
    assert!(back.return_original_content);
    assert_eq!(
        back.whitespace_only_target_policy,
        WhitespaceOnlyTargetPolicy::ExactFile
    );
}

#[test]
fn policy_serializes_as_camel_case_tags() {
    assert_eq!(
        serde_json::to_string(&WhitespaceOnlyTargetPolicy::Reject).unwrap(),
        "\"reject\""
    );
    assert_eq!(
        serde_json::to_string(&WhitespaceOnlyTargetPolicy::ExactFile).unwrap(),
        "\"exactFile\""
    );
}

#[test]
fn absent_original_content_stays_off_the_wire() {
    let result = EditBatchResult {
        path: "/tmp/a.txt".into(),
        replacements: 1,
        byte_len: 2,
        used_normalized_fallback: false,
        had_bom: false,
        crlf: false,
        old_line_count: 1,
        new_line_count: 1,
        first_changed_line: None,
        hunks: Vec::new(),
        content: None,
        original_content: None,
    };
    let json = serde_json::to_string(&result).unwrap();
    assert!(
        !json.contains("originalContent"),
        "unset field leaked into the wire: {json}"
    );

    // A response from a daemon that predates the field still deserializes.
    let legacy: EditBatchResult = serde_json::from_str(&json).unwrap();
    assert!(legacy.original_content.is_none());
}
