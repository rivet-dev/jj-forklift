use forklift::effective_status_checks;
use serde_json::json;

#[test]
fn status_checks_ignore_cancelled_run_superseded_by_success() {
    let checks = vec![
        json!({
            "name": "auto-merge-dependency-updates",
            "workflowName": "Auto Merge Dependency Updates",
            "status": "COMPLETED",
            "conclusion": "CANCELLED",
            "completedAt": "2026-07-22T07:02:06Z"
        }),
        json!({
            "name": "auto-merge-dependency-updates",
            "workflowName": "Auto Merge Dependency Updates",
            "status": "COMPLETED",
            "conclusion": "SUCCESS",
            "completedAt": "2026-07-22T07:02:24Z"
        }),
    ];

    let effective = effective_status_checks(&checks);

    assert_eq!(effective.len(), 1);
    assert_eq!(effective[0]["conclusion"], "SUCCESS");
}

#[test]
fn status_checks_keep_newer_pending_run_with_null_completion_time() {
    let checks = vec![
        json!({
            "name": "build",
            "workflowName": "CI",
            "status": "COMPLETED",
            "conclusion": "SUCCESS",
            "startedAt": "2026-07-22T07:01:00Z",
            "completedAt": "2026-07-22T07:04:00Z"
        }),
        json!({
            "name": "build",
            "workflowName": "CI",
            "status": "IN_PROGRESS",
            "conclusion": null,
            "startedAt": "2026-07-22T07:03:00Z",
            "completedAt": null
        }),
    ];

    let effective = effective_status_checks(&checks);

    assert_eq!(effective.len(), 1);
    assert_eq!(effective[0]["status"], "IN_PROGRESS");
}

#[test]
fn status_checks_keep_latest_duplicate_run_when_it_is_cancelled() {
    let checks = vec![
        json!({
            "name": "auto-merge-dependency-updates",
            "workflowName": "Auto Merge Dependency Updates",
            "status": "COMPLETED",
            "conclusion": "SUCCESS",
            "completedAt": "2026-07-22T07:02:06Z"
        }),
        json!({
            "name": "auto-merge-dependency-updates",
            "workflowName": "Auto Merge Dependency Updates",
            "status": "COMPLETED",
            "conclusion": "CANCELLED",
            "completedAt": "2026-07-22T07:02:24Z"
        }),
    ];

    let effective = effective_status_checks(&checks);

    assert_eq!(effective.len(), 1);
    assert_eq!(effective[0]["conclusion"], "CANCELLED");
}
