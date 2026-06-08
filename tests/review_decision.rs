use serde::Deserialize;

#[derive(Debug, Deserialize, PartialEq, Eq)]
struct ReviewDecisionProbe {
    #[serde(
        default,
        rename = "reviewDecision",
        deserialize_with = "forklift::empty_string_to_none"
    )]
    review_decision: Option<String>,
}

fn parse_review_decision(json: &str) -> Option<String> {
    serde_json::from_str::<ReviewDecisionProbe>(json)
        .expect("valid review decision probe JSON")
        .review_decision
}

#[test]
fn empty_review_decision_deserializes_to_none() {
    assert_eq!(parse_review_decision(r#"{"reviewDecision": ""}"#), None);
}

#[test]
fn whitespace_review_decision_deserializes_to_none() {
    assert_eq!(
        parse_review_decision(r#"{"reviewDecision": "  \n\t  "}"#),
        None
    );
}

#[test]
fn null_review_decision_deserializes_to_none() {
    assert_eq!(parse_review_decision(r#"{"reviewDecision": null}"#), None);
}

#[test]
fn missing_review_decision_deserializes_to_none() {
    assert_eq!(parse_review_decision(r#"{}"#), None);
}

#[test]
fn approved_review_decision_is_preserved() {
    assert_eq!(
        parse_review_decision(r#"{"reviewDecision": "APPROVED"}"#),
        Some("APPROVED".to_owned())
    );
}
