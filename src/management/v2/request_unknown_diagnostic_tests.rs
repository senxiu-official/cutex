use super::*;

fn fixture() -> (PathBuf, RequestIdempotencyRepository) {
    let root = std::env::temp_dir().join(format!("unknown-diag-{}", Uuid::new_v4()));
    let repo = RequestIdempotencyRepository::open(&root, "diagnostic-process".into()).unwrap();
    (root, repo)
}

#[test]
fn diagnostic_survives_reopen_without_turning_unknown_into_replay_or_retry() {
    let (root, repo) = fixture();
    let session = format!("cutex.{}", Uuid::new_v4());
    let id = Uuid::new_v4().to_string();
    let queue = Uuid::new_v4().to_string();
    let body = json!({"method":"cutex/userInput/queue/steer","params":{"queueId":queue}});
    let BeginRequest::Forward(claim) = repo.begin(&session, &id, &body).unwrap() else {
        panic!("fresh")
    };
    let diagnostic = UnknownDiagnostic::new("outcome_unknown", "native_request")
        .with_correlation(&json!({"queueId":queue,"message":"PRIVATE BODY", "token":"SECRET"}));
    repo.mark_outcome_unknown_diagnostic(&claim, &diagnostic)
        .unwrap();
    drop(repo);
    let repo = RequestIdempotencyRepository::open(&root, "after-restart".into()).unwrap();
    assert_eq!(
        repo.begin(&session, &id, &body).unwrap(),
        BeginRequest::OutcomeUnknown
    );
    assert_eq!(
        repo.begin(&session, &id, &json!({"different":true}))
            .unwrap(),
        BeginRequest::Conflict
    );
    repo.with_connection(|c| {
        let (state, status, text): (String, Option<u16>, String) = c.query_row(
            "SELECT state,response_status,response_body FROM requests WHERE request_id=?1",
            [&id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        assert_eq!(state, "outcome_unknown");
        assert_eq!(status, None);
        let v: Value = serde_json::from_str(&text)?;
        assert_eq!(v["originalCode"], "outcome_unknown");
        assert_eq!(v["correlation"]["queueId"], queue);
        assert_eq!(v["correlation"]["requestId"], id);
        assert!(!text.contains("SECRET") && !text.contains("PRIVATE"));
        Ok(())
    })
    .unwrap();
}

#[test]
fn sanitizer_is_closed_bounded_and_does_not_copy_token_shaped_ids_or_free_text() {
    let d = UnknownDiagnostic::new("SECRET", "PRIVATE BODY").with_correlation(&json!({
        "queueId":"cxm1.SECRET", "clientUserMessageId":"PRIVATE BODY", "turnId":"/private/path",
        "threadId":"a".repeat(100000), "nativeError":{"message":"SECRET"}}));
    let value = d.for_claim("browser:SECRET", "SECRET");
    assert_eq!(value["correlation"], json!({}));
    assert_eq!(value["originalCode"], "unclassified");
    assert_eq!(value["stage"], "unclassified");
    assert!(value.to_string().len() < 200);
}

#[test]
fn old_api_and_completed_claim_semantics_remain_unchanged() {
    let (_, repo) = fixture();
    let BeginRequest::Forward(claim) = repo.begin("session", "old", &json!({})).unwrap() else {
        panic!("fresh")
    };
    repo.mark_outcome_unknown(&claim).unwrap();
    repo.with_connection(|c| {
        let body: Option<String> = c.query_row(
            "SELECT response_body FROM requests WHERE request_id='old'",
            [],
            |r| r.get(0),
        )?;
        assert!(body.is_none());
        Ok(())
    })
    .unwrap();
    let BeginRequest::Forward(done) = repo.begin("session", "done", &json!({})).unwrap() else {
        panic!("fresh")
    };
    repo.complete(&done, 200, "OK", json!({"result":"done"}))
        .unwrap();
    assert!(repo
        .mark_outcome_unknown_diagnostic(&done, &UnknownDiagnostic::new("outcome_unknown", "route"))
        .is_err());
    assert!(matches!(
        repo.begin("session", "done", &json!({})).unwrap(),
        BeginRequest::Completed(_)
    ));
}
