use super::*;

// ── Export ──

#[test]
fn every_cell_survives_the_round_trip_through_the_parser() {
    let rows = vec![vec![
        "plain".to_string(),
        "has,comma".to_string(),
        "has \"quotes\"".to_string(),
        "line\nbreak".to_string(),
        "  padded  ".to_string(),
        String::new(),
    ]];
    let mut written = String::new();
    write_row(&mut written, &rows[0], ',');

    let parsed = parse_csv(&written, ',');
    assert_eq!(parsed.len(), 1, "one row in, one row out");
    assert_eq!(parsed[0], rows[0]);
}

#[test]
fn a_delimiter_that_is_not_a_comma_is_escaped_against_itself() {
    let cells = vec!["a;b".to_string(), "plain".to_string()];
    let mut written = String::new();
    write_row(&mut written, &cells, ';');
    assert_eq!(written, "\"a;b\";plain\n");
    assert_eq!(parse_csv(&written, ';')[0], cells);
}

#[tokio::test]
async fn export_renders_labels_for_options_and_leaves_currency_in_cents() {
    let state = AppState::in_memory().expect("state");
    let mut values = ValueBag::new();
    values.insert("name".to_string(), json!("Big One"));
    values.insert("stage".to_string(), json!(OPT_DEAL_STAGE_PROPOSAL));
    values.insert("amount".to_string(), json!(123_456));
    state
        .store
        .create_record(
            OBJ_DEAL,
            &CreateRecordRequest {
                values,
                created_by: None,
            },
        )
        .await
        .expect("store")
        .expect("valid");

    let (headers, body) = export_view(
        State(state.clone()),
        Path(VIEW_DEAL_ALL.to_string()),
        Query(ExportQuery::default()),
    )
    .await
    .expect("export");

    let rows = parse_csv(&body, ',');
    assert_eq!(rows.len(), 2, "header + one record");
    assert_eq!(rows[0][0], "id");
    // Headers carry field NAMES, which is what the importer's suggester matches
    // on — this is what makes an exported file re-import without hand-mapping.
    let stage_at = rows[0]
        .iter()
        .position(|h| h == "Stage")
        .expect("a Stage column");
    let amount_at = rows[0]
        .iter()
        .position(|h| h == "Amount")
        .expect("an Amount column");
    assert_eq!(
        rows[1][stage_at], "Proposal",
        "the label, not the option id"
    );
    assert_eq!(rows[1][amount_at], "123456", "cents, not dollars");

    let rows_header = headers
        .iter()
        .find(|(name, _)| name.as_str() == HDR_EXPORT_ROWS)
        .expect("row-count header");
    assert_eq!(rows_header.1, "1");
    let truncated = headers
        .iter()
        .find(|(name, _)| name.as_str() == HDR_EXPORT_TRUNCATED)
        .expect("truncation header");
    assert_eq!(truncated.1, "false");
}

#[tokio::test]
async fn exporting_an_unknown_view_is_a_404() {
    let state = AppState::in_memory().expect("state");
    let error = export_view(
        State(state.clone()),
        Path("view_nope".to_string()),
        Query(ExportQuery::default()),
    )
    .await
    .expect_err("unknown view");
    assert!(matches!(error, ApiError::NotFound(_)), "got {error:?}");
}

#[tokio::test]
async fn an_export_of_an_empty_view_is_still_a_header_row() {
    let state = AppState::in_memory().expect("state");
    let (_, body) = export_view(
        State(state.clone()),
        Path(VIEW_COMPANY_ALL.to_string()),
        Query(ExportQuery::default()),
    )
    .await
    .expect("export");
    let rows = parse_csv(&body, ',');
    assert_eq!(rows.len(), 1, "a header and nothing else");
    assert!(rows[0].contains(&"id".to_string()));
}

/// An export cut short by the row cap must SAY so — a truncated file that looks
/// complete is the only real failure mode of a ceiling.
#[tokio::test]
async fn a_capped_export_reports_that_it_was_truncated() {
    let state = AppState::in_memory().expect("state");
    for n in 0..3 {
        let mut values = ValueBag::new();
        values.insert("name".to_string(), json!(format!("Company {n}")));
        state
            .store
            .create_record(
                OBJ_COMPANY,
                &CreateRecordRequest {
                    values,
                    created_by: None,
                },
            )
            .await
            .expect("store")
            .expect("valid");
    }

    let (headers, body) = export_view(
        State(state.clone()),
        Path(VIEW_COMPANY_ALL.to_string()),
        Query(ExportQuery {
            limit: Some(2),
            ..Default::default()
        }),
    )
    .await
    .expect("export");
    assert_eq!(parse_csv(&body, ',').len(), 3, "header + two records");
    let truncated = headers
        .iter()
        .find(|(name, _)| name.as_str() == HDR_EXPORT_TRUNCATED)
        .expect("truncation header");
    assert_eq!(truncated.1, "true");
    let total = headers
        .iter()
        .find(|(name, _)| name.as_str() == HDR_EXPORT_TOTAL)
        .expect("total header");
    assert_eq!(total.1, "3", "the view's real size, not the page's");
}
