use super::*;

/// The store uses `limit` verbatim, so the ceiling exists only if the handler
/// applies it. Forwarding the caller's number compiles and looks right.
#[tokio::test]
async fn every_list_route_clamps_the_callers_limit() {
    let state = state();
    let deal = seeded_deal(&state).await;
    note_on(&state, &deal.id, "One").await;

    let timeline = list_record_activities(
        State(state.clone()),
        Path(deal.id.clone()),
        Query(TimelineQuery {
            limit: Some(100_000),
            ..TimelineQuery::default()
        }),
    )
    .await
    .unwrap()
    .0;
    assert_eq!(timeline.limit, MAX_PAGE_SIZE);

    let feed = list_activities(
        State(state.clone()),
        Query(ActivityFeedQuery {
            limit: Some(100_000),
            ..ActivityFeedQuery::default()
        }),
    )
    .await
    .unwrap()
    .0;
    assert_eq!(feed.limit, MAX_PAGE_SIZE);

    let tasks = list_tasks(
        State(state.clone()),
        Query(TaskListQuery {
            limit: Some(100_000),
            ..TaskListQuery::default()
        }),
    )
    .await
    .unwrap()
    .0;
    assert_eq!(tasks.limit, MAX_PAGE_SIZE);
}

/// A path segment that names nothing is a missing resource, not a rejected value —
/// otherwise a typo'd record id renders as "no activity yet", which is a real
/// answer for a real record.
#[tokio::test]
async fn a_timeline_on_an_unknown_record_is_a_404_not_an_empty_page() {
    let state = state();
    let err = list_record_activities(
        State(state.clone()),
        Path("rec_nope".into()),
        Query(TimelineQuery::default()),
    )
    .await
    .expect_err("an unknown record has no timeline");
    assert!(matches!(err, ApiError::NotFound(_)));

    let err = create_record_activity(
        State(state.clone()),
        Path("rec_nope".into()),
        Json(CreateActivityRequest {
            kind: ActivityKind::Note,
            title: "orphan".into(),
            ..CreateActivityRequest::default()
        }),
    )
    .await
    .expect_err("a note must not attach to a ghost");
    assert!(matches!(err, ApiError::NotFound(_)));
}

/// An entry with neither a title nor a body renders as a blank row forever. A note
/// carried only by its body is fine, though — that is how quick notes are written.
#[tokio::test]
async fn an_entirely_empty_entry_is_refused_but_a_body_only_note_is_not() {
    let state = state();
    let deal = seeded_deal(&state).await;

    let err = create_record_activity(
        State(state.clone()),
        Path(deal.id.clone()),
        Json(CreateActivityRequest {
            kind: ActivityKind::Note,
            title: "   ".into(),
            ..CreateActivityRequest::default()
        }),
    )
    .await
    .expect_err("nothing to render");
    assert!(matches!(err, ApiError::BadRequest(_)));

    let body_only = create_record_activity(
        State(state.clone()),
        Path(deal.id.clone()),
        Json(CreateActivityRequest {
            kind: ActivityKind::Note,
            body: Some("They want SSO before signing.".into()),
            ..CreateActivityRequest::default()
        }),
    )
    .await
    .expect("a body is content")
    .0;
    assert_eq!(body_only.kind, ActivityKind::Note);

    // A task, by contrast, is only ever rendered by its title.
    let err = create_task(
        State(state.clone()),
        Json(CreateTaskBody {
            body: Some("no title".into()),
            ..CreateTaskBody::default()
        }),
    )
    .await
    .expect_err("an inbox row needs a title");
    assert!(matches!(err, ApiError::BadRequest(_)));
}

/// `POST /tasks` decides the kind; the caller cannot post a note through it, and —
/// the reason for the bespoke body type — need not send a `kind` at all, which
/// `CreateActivityRequest` (whose `kind` has no serde default) would have rejected.
#[tokio::test]
async fn a_standalone_task_needs_neither_a_record_nor_a_kind() {
    let state = state();
    let parsed: CreateTaskBody =
        serde_json::from_value(json!({ "title": "Draft the renewal email" }))
            .expect("a bare title must deserialize");
    let task = create_task(State(state.clone()), Json(parsed))
        .await
        .expect("a standalone task is valid")
        .0;
    assert_eq!(task.kind, ActivityKind::Task);
    assert_eq!(task.record_id, None);

    let inbox = list_tasks(State(state.clone()), Query(TaskListQuery::default()))
        .await
        .unwrap()
        .0;
    assert_eq!(inbox.total, 1, "the default filter is `open`");
}

/// The feed's own filters, which the record-scoped route cannot express.
#[tokio::test]
async fn the_global_feed_filters_by_object_and_assignee() {
    let state = state();
    let deal = seeded_deal(&state).await;
    note_on(&state, &deal.id, "Deal note").await;
    create_task(
        State(state.clone()),
        Json(CreateTaskBody {
            record_id: Some(deal.id.clone()),
            title: "Chase legal".into(),
            assignee: Some("dana".into()),
            ..CreateTaskBody::default()
        }),
    )
    .await
    .unwrap();

    // By object SLUG, not just id — the store resolves either.
    let by_object = list_activities(
        State(state.clone()),
        Query(ActivityFeedQuery {
            object_id: Some("deal".into()),
            kinds: Some("note,task".into()),
            ..ActivityFeedQuery::default()
        }),
    )
    .await
    .unwrap()
    .0;
    assert_eq!(by_object.total, 2);

    let by_assignee = list_activities(
        State(state.clone()),
        Query(ActivityFeedQuery {
            assignee: Some("dana".into()),
            ..ActivityFeedQuery::default()
        }),
    )
    .await
    .unwrap()
    .0;
    assert_eq!(by_assignee.total, 1);

    // A blank parameter is the same as an absent one — a panel that always sends
    // the key must not end up filtering on the empty string.
    let blank = list_activities(
        State(state.clone()),
        Query(ActivityFeedQuery {
            assignee: Some("  ".into()),
            object_id: Some(String::new()),
            ..ActivityFeedQuery::default()
        }),
    )
    .await
    .unwrap()
    .0;
    assert!(blank.total >= 2);
}
