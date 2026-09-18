use super::*;

#[tokio::test]
async fn required_and_unknown_fields_are_reported_together_not_one_at_a_time() {
    let store = store().await;
    let errors = store
        .create_record(
            "deal",
            &CreateRecordRequest {
                values: bag(&[("nonsense", json!("x"))]),
                created_by: None,
            },
        )
        .await
        .unwrap()
        .expect_err("must be rejected");
    let codes: HashSet<ValidationCode> = errors.iter().map(|e| e.code).collect();
    assert!(codes.contains(&ValidationCode::UnknownField));
    assert!(codes.contains(&ValidationCode::Required));
    // name AND stage are both required on `deal`; a one-at-a-time validator would
    // have reported one.
    assert!(
        errors
            .iter()
            .filter(|e| e.code == ValidationCode::Required)
            .count()
            >= 2
    );
}

#[tokio::test]
async fn filters_and_sorts_compose_over_the_value_bag() {
    let store = store().await;
    for (name, stage, amount) in [
        ("Small", "Lead", 100_00),
        ("Medium", "Proposal", 5_000_00),
        ("Large", "Proposal", 50_000_00),
    ] {
        make_record(
            &store,
            "deal",
            &[
                ("name", json!(name)),
                ("stage", json!(stage)),
                ("amount", json!(amount)),
            ],
        )
        .await;
    }
    let page = store
        .query_records(
            &RecordQuery {
                object_id: "deal".to_string(),
                filter: Some(ViewFilter::And {
                    filters: vec![
                        ViewFilter::Condition(FilterCondition {
                            field_id: FLD_DEAL_STAGE.to_string(),
                            op: FilterOperator::IsAnyOf,
                            value: json!([OPT_DEAL_STAGE_PROPOSAL]),
                        }),
                        ViewFilter::Condition(FilterCondition {
                            field_id: "amount".to_string(),
                            op: FilterOperator::Gte,
                            value: json!(1_000_00),
                        }),
                    ],
                }),
                sorts: vec![ViewSort::desc("amount")],
                ..Default::default()
            },
            10,
            0,
        )
        .await
        .unwrap();
    assert_eq!(page.total, 2);
    assert_eq!(page.items[0].title, "Large");
    assert!(!page.has_more);

    // `search` composes with the filter tree rather than replacing it. This is
    // the FTS subquery form, distinct from `CrmStore::search`'s join — both are
    // worth a test, because the aliased spelling of exactly this is what failed
    // at runtime while compiling clean.
    let searched = store
        .query_records(
            &RecordQuery {
                object_id: "deal".to_string(),
                search: Some("Large".to_string()),
                ..Default::default()
            },
            10,
            0,
        )
        .await
        .unwrap();
    assert_eq!(searched.total, 1);
    assert_eq!(searched.items[0].title, "Large");

    // A filter naming a deleted/unknown field degrades to "no constraint" rather
    // than erroring — a saved view must survive a schema edit.
    let all = store
        .query_records(
            &RecordQuery {
                object_id: "deal".to_string(),
                filter: Some(ViewFilter::Condition(FilterCondition {
                    field_id: "fld_gone".to_string(),
                    op: FilterOperator::Eq,
                    value: json!("x"),
                })),
                ..Default::default()
            },
            10,
            0,
        )
        .await
        .unwrap();
    assert_eq!(all.total, 3);
}

#[tokio::test]
async fn duplicate_scan_finds_shared_emails_and_says_which_field_it_used() {
    let store = store().await;
    // Two records sharing an email can only exist if the unique flag is off, so
    // this exercises the scan on a non-unique field instead.
    make_record(
        &store,
        "company",
        &[("name", json!("Acme")), ("domain", json!("acme.com"))],
    )
    .await;
    make_record(
        &store,
        "company",
        &[("name", json!("ACME Inc")), ("domain", json!("ACME.com"))],
    )
    .await;
    let scan = store
        .merge_candidates(
            "company",
            &DuplicateScanRequest {
                field_ids: vec![FLD_COMPANY_DOMAIN.to_string()],
                limit: None,
            },
            20,
        )
        .await
        .unwrap();
    assert_eq!(scan.field_ids, vec![FLD_COMPANY_DOMAIN.to_string()]);
    assert_eq!(scan.candidates.len(), 1);
    assert_eq!(scan.candidates[0].record_ids.len(), 2);
    assert_eq!(scan.candidates[0].value, "acme.com");
}

#[tokio::test]
async fn board_views_group_by_status_and_always_keep_a_no_value_column() {
    let store = store().await;
    make_record(
        &store,
        "deal",
        &[
            ("name", json!("A")),
            ("stage", json!("Lead")),
            ("amount", json!(1000)),
        ],
    )
    .await;
    make_record(
        &store,
        "deal",
        &[
            ("name", json!("B")),
            ("stage", json!("Won")),
            ("amount", json!(2000)),
        ],
    )
    .await;
    let result = store
        .run_view(VIEW_DEAL_PIPELINE, &ViewQueryOverrides::default(), 20, 0)
        .await
        .unwrap()
        .unwrap();
    let groups = result.groups.expect("a board must produce groups");
    assert_eq!(groups.len(), 7, "six stages plus the no-value column");
    assert!(groups.last().unwrap().option_id.is_none());
    let won = groups
        .iter()
        .find(|g| g.option_id.as_deref() == Some(OPT_DEAL_STAGE_WON))
        .unwrap();
    assert_eq!(won.total, 1);
    assert_eq!(won.value_cents, Some(2000));
    assert_eq!(result.page.total, 2);
}

#[tokio::test]
async fn automatic_activity_kinds_cannot_be_forged() {
    let store = store().await;
    let errors = store
        .create_activity(&CreateActivityRequest {
            kind: ActivityKind::FieldChange,
            title: "I changed nothing".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .expect_err("field_change must not be user-creatable");
    assert_eq!(errors[0].code, ValidationCode::Invalid);
}

#[tokio::test]
async fn a_slug_cannot_smuggle_a_json_path_or_take_a_reserved_name() {
    let store = store().await;
    // NOTE: a mixed-case slug is LOWERCASED, not rejected — `create_field`
    // normalizes before validating, so "Stage" is accepted as "stage".
    for slug in ["created_at", "title", "a\"b", "a.b", "a b", "1a", ""] {
        let outcome = store
            .create_field(
                "company",
                None,
                &CreateFieldRequest {
                    slug: slug.into(),
                    name: "X".into(),
                    field_type: FieldType::Text,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(outcome.is_err(), "slug {slug:?} must be rejected");
    }
}
