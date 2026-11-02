use super::*;
use crate::state::MAX_PAGE_SIZE;

fn state() -> AppState {
    AppState::in_memory().expect("in-memory state")
}

/// A fixed instant so the window arithmetic is deterministic. 03:30 UTC is chosen
/// because it falls on the PREVIOUS local day for any western offset, which is
/// exactly the case a UTC-only implementation gets wrong.
fn at(raw: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(raw)
        .expect("a valid fixture instant")
        .with_timezone(&Utc)
}

async fn seeded_deal(state: &AppState) -> Record {
    let mut values = ValueBag::new();
    values.insert("name".into(), json!("Acme expansion"));
    values.insert("stage".into(), json!("Lead"));
    state
        .store
        .create_record(
            "deal",
            &CreateRecordRequest {
                values,
                created_by: None,
            },
        )
        .await
        .expect("no infrastructure error")
        .expect("seed values are valid")
}

async fn note_on(state: &AppState, record_id: &str, title: &str) -> Activity {
    create_record_activity(
        State(state.clone()),
        Path(record_id.to_string()),
        Json(CreateActivityRequest {
            kind: ActivityKind::Note,
            title: title.to_string(),
            ..CreateActivityRequest::default()
        }),
    )
    .await
    .expect("a titled note is a valid entry")
    .0
}

/// Building the router is what validates every path pattern. Two routes that
/// conflict panic HERE, at `Router::new().route(...)`, not at `cargo check`.
#[test]
fn the_router_builds_with_every_route_registered() {
    let _routes = routes();
    let _alias = router();
}

/// A chip from a newer panel must not 400 an older sidecar, so an unrecognized kind
/// is dropped and its recognized siblings still apply. The reason this is safe is
/// `ActivityKind::parse`: the tolerant `from_db` would have turned "voicemail" into
/// `note` and filtered to a kind nobody asked for.
#[test]
fn unknown_kinds_are_dropped_and_never_coerced() {
    assert_eq!(parse_kinds(None), Vec::<ActivityKind>::new());
    assert_eq!(parse_kinds(Some("")), Vec::<ActivityKind>::new());
    assert_eq!(
        parse_kinds(Some(" note , voicemail ,call")),
        vec![ActivityKind::Note, ActivityKind::Call]
    );
    // The whole point: nothing became `note` by accident.
    assert_eq!(parse_kinds(Some("voicemail")), Vec::<ActivityKind>::new());
    assert_eq!(
        parse_kinds(Some("stage_change")),
        vec![ActivityKind::StageChange]
    );
}

/// The bounds must be byte-comparable with the `due_at` column, and the local
/// offset must actually move the day boundary — a UTC-only "today" is wrong for
/// seven hours out of every day in California.
#[test]
fn task_window_bounds_track_the_local_day_and_match_the_stored_stamp_shape() {
    let now = at("2026-08-10T03:30:00Z");

    let (after, before) = task_window_bounds(now, 0, TaskWindow::Today);
    assert_eq!(after.as_deref(), Some("2026-08-10T00:00:00.000Z"));
    assert_eq!(before.as_deref(), Some("2026-08-10T23:59:59.999Z"));
    // Same width and suffix as `now_rfc3339()`, or the TEXT range scan compares
    // strings of different shapes.
    assert_eq!(after.as_deref().unwrap().len(), now_rfc3339().len());

    // UTC-7: 03:30Z is still the 9th locally, so "today" is the 9th, shifted by
    // exactly seven hours.
    let (after, before) = task_window_bounds(now, -420, TaskWindow::Today);
    assert_eq!(after.as_deref(), Some("2026-08-09T07:00:00.000Z"));
    assert_eq!(before.as_deref(), Some("2026-08-10T06:59:59.999Z"));

    // `upcoming` starts where `today` ends, so a two-section inbox shows no row
    // twice.
    let (after, before) = task_window_bounds(now, -420, TaskWindow::Upcoming);
    assert_eq!(after.as_deref(), Some("2026-08-10T07:00:00.000Z"));
    assert_eq!(before, None);

    // `overdue` is the store's own definition and gets no bound from here.
    assert_eq!(
        task_window_bounds(now, -420, TaskWindow::Overdue),
        (None, None)
    );
}

/// A nonsense offset shifts the boundary to the extreme, it does not panic and does
/// not degrade to `now` (which would make "today" start at an arbitrary moment).
#[test]
fn an_absurd_utc_offset_is_clamped_rather_than_trusted() {
    let now = at("2026-08-10T03:30:00Z");
    let (after, _) = task_window_bounds(now, 100_000, TaskWindow::Today);
    // Clamped to UTC+14, where 03:30Z is 17:30 on the 10th; local midnight is
    // 10:00Z on the 9th.
    assert_eq!(after.as_deref(), Some("2026-08-09T10:00:00.000Z"));
    // Clamped the other way to UTC-14, where 03:30Z is 13:30 on the NINTH — the
    // offset is big enough to cross back over midnight — so local midnight is
    // 14:00Z on the 9th, not the 10th.
    let (after, _) = task_window_bounds(now, -100_000, TaskWindow::Today);
    assert_eq!(after.as_deref(), Some("2026-08-09T14:00:00.000Z"));
}

/// `window` and the raw bounds are two spellings of one thing. Accepting both would
/// make one of them silently inert.
#[tokio::test]
async fn window_and_explicit_bounds_are_mutually_exclusive() {
    let err = list_tasks(
        State(state()),
        Query(TaskListQuery {
            window: Some(TaskWindow::Today),
            due_before: Some("2026-01-01T00:00:00.000Z".into()),
            ..TaskListQuery::default()
        }),
    )
    .await
    .expect_err("both spellings at once must be refused");
    assert!(matches!(err, ApiError::BadRequest(_)));
}

/// The regression the `Option<TaskFilter>` in `TaskListQuery` exists to prevent: if
/// the filter were `TaskFilter` with a serde default, it would always be `Some`-like
/// and the conflict check above would fire on every plain window query.
#[tokio::test]
async fn a_plain_window_query_is_accepted() {
    let state = state();
    let now = Utc::now();
    let due_today = create_task(
        State(state.clone()),
        Json(CreateTaskBody {
            title: "Call Bob".into(),
            due_at: Some(stamp(now)),
            ..CreateTaskBody::default()
        }),
    )
    .await
    .expect("a titled task is valid")
    .0;
    let due_next_week = create_task(
        State(state.clone()),
        Json(CreateTaskBody {
            title: "Renewal review".into(),
            due_at: Some(stamp(now + Duration::days(7))),
            ..CreateTaskBody::default()
        }),
    )
    .await
    .expect("a titled task is valid")
    .0;

    let today = list_tasks(
        State(state.clone()),
        Query(TaskListQuery {
            window: Some(TaskWindow::Today),
            ..TaskListQuery::default()
        }),
    )
    .await
    .expect("a window alone is a well-formed query")
    .0;
    let ids: Vec<&str> = today.items.iter().map(|a| a.id.as_str()).collect();
    assert_eq!(ids, vec![due_today.id.as_str()]);

    let upcoming = list_tasks(
        State(state.clone()),
        Query(TaskListQuery {
            window: Some(TaskWindow::Upcoming),
            ..TaskListQuery::default()
        }),
    )
    .await
    .unwrap()
    .0;
    let ids: Vec<&str> = upcoming.items.iter().map(|a| a.id.as_str()).collect();
    assert_eq!(ids, vec![due_next_week.id.as_str()]);
}

/// The audit-trail guard. `store::update_activity` and `store::delete_activity` are
/// kind-blind, so without this a user can rewrite the history of a deal — and
/// deleting a `stage_change` is not merely cosmetic: `funnel_report` reconstructs
/// every stage path in memory from exactly those rows, so one delete silently
/// changes what the pipeline report says happened.
#[tokio::test]
async fn entries_the_store_wrote_itself_cannot_be_edited_or_deleted() {
    let state = state();
    let deal = seeded_deal(&state).await;

    let mut moved = ValueBag::new();
    moved.insert("stage".into(), json!("Proposal"));
    state
        .store
        .update_record(
            &deal.id,
            &UpdateRecordRequest {
                values: moved,
                mode: UpdateMode::Merge,
            },
        )
        .await
        .expect("no infrastructure error")
        .expect("a known option label is valid")
        .expect("the record exists");

    let automatic = list_record_activities(
        State(state.clone()),
        Path(deal.id.clone()),
        Query(TimelineQuery {
            kinds: Some("stage_change,field_change".into()),
            ..TimelineQuery::default()
        }),
    )
    .await
    .unwrap()
    .0;
    assert!(
        !automatic.items.is_empty(),
        "moving a status field must leave an automatic entry to protect"
    );

    for entry in &automatic.items {
        let err = patch_activity(
            State(state.clone()),
            Path(entry.id.clone()),
            Json(UpdateActivityRequest {
                title: Some("never happened".into()),
                ..UpdateActivityRequest::default()
            }),
        )
        .await
        .expect_err("an automatic entry must not be editable");
        assert!(matches!(err, ApiError::Conflict(_)));

        let err = delete_activity(State(state.clone()), Path(entry.id.clone()))
            .await
            .expect_err("an automatic entry must not be deletable");
        assert!(matches!(err, ApiError::Conflict(_)));
    }

    // Positive control: the guard must not have locked the whole timeline.
    let note = note_on(&state, &deal.id, "Left a voicemail").await;
    let edited = patch_activity(
        State(state.clone()),
        Path(note.id.clone()),
        Json(UpdateActivityRequest {
            title: Some("Spoke to Dana".into()),
            ..UpdateActivityRequest::default()
        }),
    )
    .await
    .expect("an authored note is editable")
    .0;
    assert_eq!(edited.title, "Spoke to Dana");
    delete_activity(State(state.clone()), Path(note.id))
        .await
        .expect("an authored note is deletable");
}

/// `store::complete_task` scopes its UPDATE to `kind = 'task'` and then returns
/// whatever row it found, so without this guard completing a note answers 200 with
/// an unchanged note — a success response for work that did not happen.
#[tokio::test]
async fn completing_something_that_is_not_a_task_is_refused_not_ignored() {
    let state = state();
    let deal = seeded_deal(&state).await;
    let note = note_on(&state, &deal.id, "Kickoff summary").await;

    let err = complete_activity(State(state.clone()), Path(note.id.clone()), None)
        .await
        .expect_err("a note has nothing to complete");
    assert!(matches!(err, ApiError::Conflict(_)));

    let task = create_task(
        State(state.clone()),
        Json(CreateTaskBody {
            record_id: Some(deal.id.clone()),
            title: "Send the proposal".into(),
            ..CreateTaskBody::default()
        }),
    )
    .await
    .unwrap()
    .0;

    // A bodiless click completes. The derived `Default` for `CompleteTaskRequest`
    // is `completed: false`, so a handler written with `unwrap_or_default()` would
    // pass this assertion's opposite.
    let done = complete_activity(State(state.clone()), Path(task.id.clone()), None)
        .await
        .unwrap()
        .0;
    assert!(done.completed_at.is_some());

    let reopened = complete_activity(
        State(state.clone()),
        Path(task.id),
        Some(Json(CompleteTaskRequest { completed: false })),
    )
    .await
    .unwrap()
    .0;
    assert!(reopened.completed_at.is_none());
}

mod lists;
