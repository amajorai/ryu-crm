use super::*;

#[tokio::test]
async fn seeding_is_idempotent_and_lands_the_five_standard_objects() {
    let store = store().await;
    let objects = store.list_objects().await.unwrap();
    let slugs: Vec<&str> = objects.iter().map(|o| o.slug.as_str()).collect();
    assert_eq!(slugs, vec!["company", "person", "deal", "note", "task"]);
    assert!(objects.iter().all(|o| o.is_standard));
    assert!(objects.iter().all(|o| o.title_field_id.is_some()));

    // Every seeded object must have a default view, or opening it has no target.
    for object in &objects {
        let views = store.list_views(&object.id).await.unwrap();
        assert!(!views.is_empty(), "{} has no views", object.slug);
        assert_eq!(
            views.iter().filter(|v| v.is_default).count(),
            1,
            "{} must have exactly one default view",
            object.slug
        );
    }
    let deal_fields = store.list_fields(OBJ_DEAL).await.unwrap();
    assert!(deal_fields.iter().any(|f| f.id == FLD_DEAL_STAGE));
    assert_eq!(
        deal_fields
            .iter()
            .find(|f| f.id == FLD_DEAL_STAGE)
            .unwrap()
            .config
            .options
            .len(),
        6
    );
}

#[tokio::test]
async fn unique_fields_reject_a_second_record_but_tolerate_blanks_and_self() {
    let store = store().await;
    let first = make_record(
        &store,
        "person",
        &[("name", json!("Jane")), ("email", json!("jane@acme.com"))],
    )
    .await;
    let clash = store
        .create_record(
            "person",
            &CreateRecordRequest {
                values: bag(&[("name", json!("J. Doe")), ("email", json!("JANE@acme.com"))]),
                created_by: None,
            },
        )
        .await
        .unwrap()
        .expect_err("duplicate email must be rejected");
    assert_eq!(clash[0].code, ValidationCode::NotUnique);

    // Two people with no email are not duplicates.
    make_record(&store, "person", &[("name", json!("Nobody One"))]).await;
    make_record(&store, "person", &[("name", json!("Nobody Two"))]).await;

    // Re-saving the same record with its own email must not collide with itself.
    let update = store
        .update_record(
            &first.id,
            &UpdateRecordRequest {
                values: bag(&[
                    ("email", json!("jane@acme.com")),
                    ("job_title", json!("CTO")),
                ]),
                mode: UpdateMode::Merge,
            },
        )
        .await
        .unwrap()
        .expect("self-collision must not be reported");
    assert!(update
        .unwrap()
        .changed
        .iter()
        .any(|c| c.field_slug == "job_title"));
}

#[tokio::test]
async fn search_finds_records_and_survives_operator_characters() {
    let store = store().await;
    make_record(
        &store,
        "company",
        &[
            ("name", json!("Acme Industries")),
            ("description", json!("A widget manufacturer")),
        ],
    )
    .await;
    make_record(&store, "company", &[("name", json!("Globex"))]).await;

    let hits = store
        .search(
            &SearchQuery {
                query: "widget".into(),
                ..Default::default()
            },
            10,
            0,
        )
        .await
        .unwrap();
    assert_eq!(hits.total, 1);
    assert_eq!(hits.hits[0].title, "Acme Industries");
    assert_eq!(hits.hits[0].object_slug, "company");

    // Prefix matching as you type.
    let prefix = store
        .search(
            &SearchQuery {
                query: "acm".into(),
                ..Default::default()
            },
            10,
            0,
        )
        .await
        .unwrap();
    assert_eq!(prefix.total, 1);

    // Raw FTS5 syntax must not reach the matcher.
    let hostile = store
        .search(
            &SearchQuery {
                query: "acme OR \"".into(),
                ..Default::default()
            },
            10,
            0,
        )
        .await
        .unwrap();
    assert!(hostile.total <= 1);
}

#[tokio::test]
async fn csv_import_previews_then_applies_the_same_plan() {
    let store = store().await;
    make_record(
        &store,
        "person",
        &[
            ("name", json!("Jane Doe")),
            ("email", json!("jane@acme.com")),
        ],
    )
    .await;

    let csv = "Name,Email,Job title\nJane Doe,jane@acme.com,CTO\nJohn Roe,john@acme.com,CEO\n\"Quoted, Name\",quoted@acme.com,\n";
    let job = store
        .create_import(
            &CreateImportRequest {
                object_id: "person".into(),
                filename: Some("people.csv".into()),
                csv: csv.to_string(),
                delimiter: None,
                has_header: None,
            },
            MAX_TEST_IMPORT,
        )
        .await
        .unwrap()
        .unwrap();
    assert!(job.has_header);
    assert_eq!(job.row_count, 3);
    assert_eq!(job.columns.len(), 3);
    // The header names match the seeded fields, so the mapping pre-fills.
    assert_eq!(
        job.columns[0].suggested_field_id.as_deref(),
        Some(FLD_PERSON_NAME)
    );
    assert_eq!(
        job.columns[2].suggested_field_id.as_deref(),
        Some(FLD_PERSON_JOB_TITLE)
    );

    store
        .set_import_mapping(
            &job.id,
            &SetImportMappingRequest {
                mappings: job.mappings.clone(),
                dedupe: ImportDedupe {
                    match_field_ids: vec![FLD_PERSON_EMAIL.to_string()],
                    strategy: DedupeStrategy::FillBlanks,
                },
            },
        )
        .await
        .unwrap()
        .unwrap();

    let preview = store.dry_run_import(&job.id).await.unwrap().unwrap();
    assert_eq!(preview.total_rows, 3);
    assert_eq!(preview.create_count, 2);
    assert_eq!(
        preview.update_count, 1,
        "the existing Jane gets her blank job title filled"
    );
    assert_eq!(preview.error_count, 0);
    assert!(preview.unmapped_columns.is_empty());

    let result = store.apply_import(&job.id).await.unwrap().unwrap();
    assert_eq!(result.created, 2);
    assert_eq!(result.updated, 1);
    assert_eq!(result.created_record_ids.len(), 2);
    assert_eq!(
        store
            .query_records(
                &RecordQuery {
                    object_id: "person".into(),
                    ..Default::default()
                },
                50,
                0
            )
            .await
            .unwrap()
            .total,
        3
    );
    // The quoted cell survived the parser intact.
    let hits = store
        .search(
            &SearchQuery {
                query: "Quoted".into(),
                ..Default::default()
            },
            10,
            0,
        )
        .await
        .unwrap();
    assert_eq!(hits.total, 1);
    // Re-applying is refused rather than silently duplicating.
    assert!(store.apply_import(&job.id).await.is_err());
}

#[tokio::test]
async fn pipeline_report_counts_unassigned_rather_than_dropping_it() {
    let store = store().await;
    // `stage` is required on `deal`, so an unassigned row can only be made on an
    // object whose status field is optional — `task` is exactly that.
    make_record(
        &store,
        "task",
        &[("name", json!("A")), ("status", json!("Done"))],
    )
    .await;
    make_record(
        &store,
        "task",
        &[("name", json!("B")), ("status", json!("Cancelled"))],
    )
    .await;
    make_record(&store, "task", &[("name", json!("C"))]).await;
    let report = store
        .pipeline_report(&PipelineRequest {
            object_id: Some("task".into()),
            include_closed: true,
            ..Default::default()
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(report.total_records, 3);
    assert_eq!(report.unassigned_count, 1);
    assert_eq!(report.won_count, 1);
    assert_eq!(report.lost_count, 1);
    assert!((report.win_rate - 0.5).abs() < f64::EPSILON);
}

#[tokio::test]
async fn custom_objects_arrive_usable_and_delete_cleanly() {
    let store = store().await;
    let object = store
        .create_object(&CreateObjectRequest {
            slug: "product".into(),
            singular: "Product".into(),
            plural: None,
            icon: None,
            description: None,
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(object.plural, "Products");
    assert!(!object.is_standard);
    // A new object must be immediately writable and openable.
    let fields = store.list_fields(&object.id).await.unwrap();
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0].slug, "name");
    assert_eq!(
        object.title_field_id.as_deref(),
        Some(fields[0].id.as_str())
    );
    assert_eq!(store.list_views(&object.id).await.unwrap().len(), 1);

    let record = make_record(&store, "product", &[("name", json!("Widget"))]).await;
    assert_eq!(record.title, "Widget");
    assert!(store.delete_object(&object.id).await.unwrap());
    assert!(store.get_record(&record.id).await.unwrap().is_none());
    // Its FTS rows go with it.
    assert_eq!(
        store
            .search(
                &SearchQuery {
                    query: "Widget".into(),
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

#[tokio::test]
async fn schema_boot_call_returns_everything_the_panel_needs() {
    let store = store().await;
    let schema = store.schema().await.unwrap();
    assert_eq!(schema.objects.len(), 5);
    let deal = schema
        .objects
        .iter()
        .find(|o| o.object.slug == "deal")
        .unwrap();
    assert!(deal.fields.len() >= 10);
    assert_eq!(deal.views.len(), 2);
    assert_eq!(deal.record_count, 0);
}
