use super::*;

#[tokio::test]
async fn currency_is_cents_when_integer_and_major_units_otherwise() {
    let store = store().await;
    let panel = make_record(
        &store,
        "deal",
        &[
            ("name", json!("A")),
            ("stage", json!("Lead")),
            ("amount", json!(12345)),
        ],
    )
    .await;
    assert_eq!(panel.values["amount"], json!(12345));
    let csv = make_record(
        &store,
        "deal",
        &[
            ("name", json!("B")),
            ("stage", json!("Lead")),
            ("amount", json!("$1,234.56")),
        ],
    )
    .await;
    assert_eq!(csv.values["amount"], json!(123456));
    let agent = make_record(
        &store,
        "deal",
        &[
            ("name", json!("C")),
            ("stage", json!("Lead")),
            ("amount", json!(99.5)),
        ],
    )
    .await;
    assert_eq!(agent.values["amount"], json!(9950));
}

#[tokio::test]
async fn relations_are_queryable_from_both_ends_with_the_right_label() {
    let store = store().await;
    let acme = make_record(&store, "company", &[("name", json!("Acme"))]).await;
    let jane = make_record(
        &store,
        "person",
        &[("name", json!("Jane")), ("company", json!(acme.id.clone()))],
    )
    .await;

    let outgoing = store.list_links(&jane.id).await.unwrap();
    assert_eq!(outgoing.len(), 1);
    assert_eq!(outgoing[0].direction, LinkDirection::Outgoing);
    assert_eq!(outgoing[0].record_id, acme.id);
    assert_eq!(outgoing[0].label, "Company");

    let incoming = store.list_links(&acme.id).await.unwrap();
    assert_eq!(incoming.len(), 1);
    assert_eq!(incoming[0].direction, LinkDirection::Incoming);
    assert_eq!(incoming[0].record_id, jane.id);
    // The inverse label, NOT the forward field's name — otherwise Acme's page
    // would read "Company: Jane".
    assert_eq!(incoming[0].label, "People");

    // A relation to a record that does not exist is rejected, not stored.
    let bad = store
        .create_record(
            "person",
            &CreateRecordRequest {
                values: bag(&[("name", json!("Ghost")), ("company", json!("rec_nope"))]),
                created_by: None,
            },
        )
        .await
        .unwrap()
        .expect_err("dangling relation must be rejected");
    assert_eq!(bad[0].code, ValidationCode::BadRelationTarget);
}

#[tokio::test]
async fn soft_delete_hides_a_record_from_search_and_queries_until_restored() {
    let store = store().await;
    let company = make_record(&store, "company", &[("name", json!("Initech"))]).await;
    assert!(store.delete_record(&company.id).await.unwrap());
    let visible = store
        .query_records(
            &RecordQuery {
                object_id: "company".into(),
                ..Default::default()
            },
            10,
            0,
        )
        .await
        .unwrap();
    assert_eq!(visible.total, 0);
    assert_eq!(
        store
            .search(
                &SearchQuery {
                    query: "Initech".into(),
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
    assert!(store.restore_record(&company.id).await.unwrap());
    assert_eq!(
        store
            .query_records(
                &RecordQuery {
                    object_id: "company".into(),
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
}

#[tokio::test]
async fn import_rows_that_fail_validation_do_not_block_the_rest() {
    let store = store().await;
    let csv = "Name,Email\nGood One,good@acme.com\nBad One,not-an-email\n";
    let job = store
        .create_import(
            &CreateImportRequest {
                object_id: "person".into(),
                csv: csv.to_string(),
                ..Default::default()
            },
            MAX_TEST_IMPORT,
        )
        .await
        .unwrap()
        .unwrap();
    let preview = store.dry_run_import(&job.id).await.unwrap().unwrap();
    assert_eq!(preview.create_count, 1);
    assert_eq!(preview.error_count, 1);
    // The failing row must be IN the samples — a preview that hides failures is
    // worse than none.
    assert!(preview
        .samples
        .iter()
        .any(|s| s.action == ImportAction::Error));
    let result = store.apply_import(&job.id).await.unwrap().unwrap();
    assert_eq!(result.created, 1);
    assert_eq!(result.failed, 1);
}

#[tokio::test]
async fn funnel_report_credits_records_created_straight_into_a_stage() {
    let store = store().await;
    let deal = make_record(
        &store,
        "deal",
        &[("name", json!("A")), ("stage", json!("Lead"))],
    )
    .await;
    store
        .update_record(
            &deal.id,
            &UpdateRecordRequest {
                values: bag(&[("stage", json!("Qualified"))]),
                mode: UpdateMode::Merge,
            },
        )
        .await
        .unwrap()
        .unwrap();
    make_record(
        &store,
        "deal",
        &[("name", json!("B")), ("stage", json!("Lead"))],
    )
    .await;

    let report = store
        .funnel_report(&FunnelRequest::default())
        .await
        .unwrap()
        .unwrap();
    let lead = report
        .steps
        .iter()
        .find(|s| s.option_id == OPT_DEAL_STAGE_LEAD)
        .unwrap();
    assert_eq!(lead.entered, 2, "both deals were in Lead");
    assert_eq!(lead.advanced, 1);
    assert!((lead.conversion_rate - 0.5).abs() < f64::EPSILON);
    let qualified = report
        .steps
        .iter()
        .find(|s| s.option_id == OPT_DEAL_STAGE_QUALIFIED)
        .unwrap();
    assert_eq!(qualified.entered, 1);
    assert_eq!(qualified.advanced, 0);
}

#[tokio::test]
async fn standard_objects_and_system_fields_are_undeletable() {
    let store = store().await;
    assert!(store.delete_object(OBJ_DEAL).await.is_err());
    assert!(store.delete_field(FLD_DEAL_NAME).await.is_err());
}

#[tokio::test]
async fn a_replace_update_clears_the_fields_it_omits() {
    let store = store().await;
    let deal = make_record(
        &store,
        "deal",
        &[
            ("name", json!("Acme")),
            ("stage", json!("Lead")),
            ("probability", json!(40)),
        ],
    )
    .await;
    let update = store
        .update_record(
            &deal.id,
            &UpdateRecordRequest {
                values: bag(&[("name", json!("Acme")), ("stage", json!("Lead"))]),
                mode: UpdateMode::Replace,
            },
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(!update.record.values.contains_key("probability"));
    // The same payload as a MERGE would have left it alone.
    assert!(update.changed.iter().any(|c| c.field_slug == "probability"));
}
