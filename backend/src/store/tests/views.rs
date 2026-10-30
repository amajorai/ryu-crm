use super::*;

#[tokio::test]
async fn select_values_accept_labels_and_normalize_to_option_ids() {
    let store = store().await;
    let deal = make_record(
        &store,
        "deal",
        &[("name", json!("Acme")), ("stage", json!("Proposal"))],
    )
    .await;
    assert_eq!(deal.values["stage"], json!(OPT_DEAL_STAGE_PROPOSAL));
    assert_eq!(deal.title, "Acme");
}

#[tokio::test]
async fn updates_diff_status_fields_and_write_a_stage_change_activity() {
    let store = store().await;
    let deal = make_record(
        &store,
        "deal",
        &[("name", json!("Acme")), ("stage", json!("Lead"))],
    )
    .await;
    let update = store
        .update_record(
            &deal.id,
            &UpdateRecordRequest {
                values: bag(&[("stage", json!("Won"))]),
                mode: UpdateMode::Merge,
            },
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let stage = update.stage_change.expect("a status move must be surfaced");
    assert_eq!(stage.from.as_deref(), Some(OPT_DEAL_STAGE_LEAD));
    assert_eq!(stage.to.as_deref(), Some(OPT_DEAL_STAGE_WON));
    assert_eq!(stage.to_label.as_deref(), Some("Won"));

    let timeline = store
        .query_activities(
            &ActivityQuery {
                record_id: Some(deal.id.clone()),
                ..Default::default()
            },
            50,
            0,
        )
        .await
        .unwrap();
    assert!(timeline
        .items
        .iter()
        .any(|a| a.kind == ActivityKind::StageChange));
    assert!(timeline
        .items
        .iter()
        .any(|a| a.kind == ActivityKind::FieldChange));

    // A no-op PATCH must produce no diff, so the caller does not emit an event.
    let noop = store
        .update_record(
            &deal.id,
            &UpdateRecordRequest {
                values: bag(&[("stage", json!("Won"))]),
                mode: UpdateMode::Merge,
            },
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(noop.changed.is_empty());
}

#[tokio::test]
async fn merge_fills_blanks_moves_history_and_never_silently_overwrites() {
    let store = store().await;
    let survivor = make_record(
        &store,
        "person",
        &[
            ("name", json!("Jane Doe")),
            ("email", json!("jane@acme.com")),
        ],
    )
    .await;
    let loser = make_record(
        &store,
        "person",
        &[("name", json!("J. Doe")), ("phone", json!("+1 555 0100"))],
    )
    .await;
    store
        .create_activity(&CreateActivityRequest {
            record_id: Some(loser.id.clone()),
            kind: ActivityKind::Note,
            title: "Met at a conference".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .unwrap();

    let plan = MergePlan {
        survivor_id: survivor.id.clone(),
        loser_ids: vec![loser.id.clone()],
        resolutions: Vec::new(),
        soft_delete_losers: true,
    };
    let preview = store.plan_merge(&plan).await.unwrap().unwrap();
    assert_eq!(preview.activity_count, 1);
    // The names differ and the survivor's is non-empty ⇒ a conflict the user must
    // see, and the default keeps the survivor's.
    assert!(preview.conflicts.iter().any(|c| c.field_slug == "name"));
    assert_eq!(preview.resolved_values["name"], json!("Jane Doe"));
    // The blank phone is filled from the loser without being asked.
    assert_eq!(preview.resolved_values["phone"], json!("+1 555 0100"));

    let outcome = store.merge_records(&plan).await.unwrap().unwrap();
    assert_eq!(outcome.merged_record_ids, vec![loser.id.clone()]);
    assert_eq!(outcome.moved_activities, 1);
    assert_eq!(outcome.survivor.values["phone"], json!("+1 555 0100"));
    assert!(store
        .get_record(&loser.id)
        .await
        .unwrap()
        .unwrap()
        .deleted_at
        .is_some());
}

#[tokio::test]
async fn lists_carry_their_own_fields_separate_from_the_records() {
    let store = store().await;
    let deal = make_record(
        &store,
        "deal",
        &[("name", json!("Acme")), ("stage", json!("Lead"))],
    )
    .await;
    let list = store
        .create_list(&CreateListRequest {
            object_id: "deal".into(),
            name: "Q3 push".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    // A list field named `stage` must NOT collide with the deal's own `stage`.
    let list_field = store
        .create_field(
            "deal",
            Some(&list.id),
            &CreateFieldRequest {
                slug: "stage".into(),
                name: "List stage".into(),
                field_type: FieldType::Select,
                config: FieldConfig {
                    options: vec![
                        SelectOption::new("", "Shortlisted", 0),
                        SelectOption::new("", "Dropped", 1),
                    ],
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(list_field.list_id.as_deref(), Some(list.id.as_str()));
    assert!(list_field.config.options[0].id.starts_with("opt_stage_"));

    // A unique LIST field would enforce nothing (uniqueness is checked against
    // `records`, not `list_entries`), so it is rejected rather than accepted and
    // quietly inert.
    assert!(store
        .create_field(
            "deal",
            Some(&list.id),
            &CreateFieldRequest {
                slug: "ref_code".into(),
                name: "Ref".into(),
                field_type: FieldType::Text,
                is_unique: true,
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .is_err());

    let entry = store
        .add_list_entry(
            &list.id,
            &AddListEntryRequest {
                record_id: deal.id.clone(),
                values: bag(&[("stage", json!("Shortlisted"))]),
            },
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        entry.values["stage"],
        json!(list_field.config.options[0].id)
    );

    let page = store
        .query_list_entries(
            &ListEntryQuery {
                list_id: list.id.clone(),
                ..Default::default()
            },
            10,
            0,
        )
        .await
        .unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(page.items[0].record.title, "Acme");
    // The record's OWN stage is untouched by the list's.
    assert_eq!(
        page.items[0].record.values["stage"],
        json!(OPT_DEAL_STAGE_LEAD)
    );

    let detail = store.get_record_detail(&deal.id).await.unwrap().unwrap();
    assert_eq!(detail.lists.len(), 1);
    assert_eq!(detail.lists[0].list_name, "Q3 push");
}

#[tokio::test]
async fn due_tasks_are_claimed_exactly_once() {
    let store = store().await;
    let record = make_record(
        &store,
        "deal",
        &[("name", json!("Acme")), ("stage", json!("Lead"))],
    )
    .await;
    store
        .create_activity(&CreateActivityRequest {
            record_id: Some(record.id.clone()),
            kind: ActivityKind::Task,
            title: "Call them back".into(),
            due_at: Some("2020-01-01T00:00:00Z".into()),
            ..Default::default()
        })
        .await
        .unwrap()
        .unwrap();
    store
        .create_activity(&CreateActivityRequest {
            record_id: Some(record.id.clone()),
            kind: ActivityKind::Task,
            title: "Not due for ages".into(),
            due_at: Some("2999-01-01T00:00:00Z".into()),
            ..Default::default()
        })
        .await
        .unwrap()
        .unwrap();

    let first = store.claim_due_tasks(10).await.unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].title, "Call them back");
    assert!(first[0].due_notified_at.is_some());
    // The claim IS the stamp: a second sweep must find nothing.
    assert!(store.claim_due_tasks(10).await.unwrap().is_empty());

    let overdue = store
        .list_tasks(
            &TaskQuery {
                filter: TaskFilter::Overdue,
                ..Default::default()
            },
            10,
            0,
        )
        .await
        .unwrap();
    assert_eq!(overdue.total, 1);
}

#[tokio::test]
async fn deleting_a_field_strips_it_from_every_record_and_from_search() {
    let store = store().await;
    let field = store
        .create_field(
            "company",
            None,
            &CreateFieldRequest {
                slug: "internal_code".into(),
                name: "Internal code".into(),
                field_type: FieldType::Text,
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .unwrap();
    let record = make_record(
        &store,
        "company",
        &[("name", json!("Acme")), ("internal_code", json!("ZX9000"))],
    )
    .await;
    assert_eq!(
        store
            .search(
                &SearchQuery {
                    query: "ZX9000".into(),
                    ..Default::default()
                },
                10,
                0
            )
            .await
            .unwrap()
            .total,
        1
    );
    assert!(store.delete_field(&field.id).await.unwrap());
    let after = store.get_record(&record.id).await.unwrap().unwrap();
    assert!(!after.values.contains_key("internal_code"));
    assert_eq!(
        store
            .search(
                &SearchQuery {
                    query: "ZX9000".into(),
                    ..Default::default()
                },
                10,
                0
            )
            .await
            .unwrap()
            .total,
        0
    );
}
