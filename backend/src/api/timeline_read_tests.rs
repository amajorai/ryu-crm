//! Read paths an agent or panel leans on: one activity by id, an object's field
//! list, and the `tools/search` sentence-plus-rows answer.

use super::*;
use crate::api::insights::tool_search;
use crate::api::objects::list_fields;

fn state() -> AppState {
    AppState::in_memory().expect("in-memory state")
}

async fn seed(state: &AppState, object: &str, name: &str) -> Record {
    let mut values = ValueBag::new();
    values.insert("name".into(), json!(name));
    if object == "deal" {
        values.insert("stage".into(), json!("Lead"));
    }
    state
        .store
        .create_record(
            object,
            &CreateRecordRequest {
                values,
                created_by: None,
            },
        )
        .await
        .expect("no infrastructure error")
        .expect("seed values are valid")
}

async fn search(state: &AppState, query: &str, object: Option<&str>) -> ToolResponse {
    tool_search(
        State(state.clone()),
        Json(ToolSearchRequest {
            query: query.to_string(),
            object: object.map(str::to_string),
            limit: None,
        }),
    )
    .await
    .expect("a tool call never surfaces a correctable mistake as an HTTP error")
    .0
}

#[tokio::test]
async fn one_activity_reads_back_in_full_and_an_unknown_one_is_a_404() {
    let state = state();
    let deal = seed(&state, "deal", "Acme expansion").await;
    let created = create_record_activity(
        State(state.clone()),
        Path(deal.id.clone()),
        Json(CreateActivityRequest {
            kind: ActivityKind::Note,
            title: "Kickoff".to_string(),
            body: Some("Agreed on scope".to_string()),
            ..CreateActivityRequest::default()
        }),
    )
    .await
    .expect("a titled note is valid")
    .0;
    // A later entry on the same record must not shadow the one asked for.
    create_record_activity(
        State(state.clone()),
        Path(deal.id.clone()),
        Json(CreateActivityRequest {
            kind: ActivityKind::Note,
            title: "Follow-up".to_string(),
            body: Some("Sent the proposal".to_string()),
            ..CreateActivityRequest::default()
        }),
    )
    .await
    .expect("a titled note is valid")
    .0;

    let read = get_activity(State(state.clone()), Path(created.id.clone()))
        .await
        .expect("the entry exists")
        .0;
    assert_eq!(
        serde_json::to_value(&read).unwrap(),
        serde_json::to_value(&created).unwrap()
    );
    assert_eq!(read.record_id.as_deref(), Some(deal.id.as_str()));
    assert_eq!(read.title, "Kickoff");

    let error = get_activity(State(state.clone()), Path("act_missing".to_string()))
        .await
        .expect_err("no such entry");
    assert!(matches!(error, ApiError::NotFound(_)), "{error}");
}

#[tokio::test]
async fn an_objects_fields_list_the_same_by_slug_or_id() {
    let state = state();
    let by_slug = list_fields(State(state.clone()), Path("person".to_string()))
        .await
        .expect("person is a seeded object")
        .0;
    let by_id = list_fields(State(state.clone()), Path(OBJ_PERSON.to_string()))
        .await
        .expect("the id resolves too")
        .0;
    let ids = |fields: &[Field]| fields.iter().map(|f| f.id.clone()).collect::<Vec<_>>();
    assert_eq!(ids(&by_slug), ids(&by_id));
    assert!(by_slug.iter().all(|f| f.object_id == OBJ_PERSON));
    assert!(ids(&by_slug).contains(&FLD_PERSON_EMAIL.to_string()));
    let company = list_fields(State(state.clone()), Path("company".to_string()))
        .await
        .expect("company is seeded")
        .0;
    assert!(!ids(&company).contains(&FLD_PERSON_EMAIL.to_string()));
}

#[tokio::test]
async fn tool_search_answers_with_ranked_hits_and_a_sentence() {
    let state = state();
    seed(&state, "company", "Acme Corporation").await;
    seed(&state, "company", "Globex").await;
    seed(&state, "deal", "Acme renewal").await;

    let all = search(&state, "Acme", None).await;
    assert!(all.ok);
    let data = all.data.expect("rows");
    let hits = data["hits"].as_array().expect("hits");
    assert_eq!(data["total"], json!(2));
    assert_eq!(hits.len(), 2);
    let ranks: Vec<f64> = hits.iter().map(|h| h["rank"].as_f64().unwrap()).collect();
    assert!(
        ranks.windows(2).all(|w| w[0] <= w[1]),
        "best (lowest bm25) first: {ranks:?}"
    );
    assert!(
        all.summary.starts_with("Found 2 records matching \"Acme\""),
        "{}",
        all.summary
    );
    assert!(all.summary.contains("Acme Corporation") && all.summary.contains("Acme renewal"));
    assert!(!all.summary.contains("Globex"));

    let deals = search(&state, "Acme", Some("deal")).await;
    let data = deals.data.expect("rows");
    assert_eq!(data["total"], json!(1));
    assert_eq!(data["hits"][0]["object_slug"], json!("deal"));

    let none = search(&state, "Initech", None).await;
    assert!(none.ok);
    assert_eq!(none.summary, "No records match \"Initech\".");

    let blank = search(&state, "   ", None).await;
    assert!(!blank.ok, "an empty query is a correctable mistake");
    assert_eq!(blank.summary, "a search needs a query");
}
