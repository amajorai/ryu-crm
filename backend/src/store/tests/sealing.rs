//! At-rest sealing: parity with the plaintext behaviour, bytes on disk, legacy
//! migration, AAD binding and the key states.

use super::*;
use crate::seal_keys::{KeySource, PluginKeys};

const SECRET: &str = "zebracrmmarker";

fn keys(n: u8) -> PluginKeys {
    use base64::Engine as _;
    let subkey = base64::engine::general_purpose::STANDARD.encode([n; 32]);
    PluginKeys::sealing(
        ryu_crypto::PluginCipher::from_subkey_b64(PLUGIN_ID, &subkey).unwrap(),
        KeySource::Delivered,
    )
}

fn db_path() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ryu-crm-seal-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("crm.db")
}

fn disk_has(path: &std::path::Path, needle: &str) -> bool {
    let mut bytes = std::fs::read(path).unwrap_or_default();
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    bytes.extend(std::fs::read(PathBuf::from(wal)).unwrap_or_default());
    bytes.windows(needle.len()).any(|w| w == needle.as_bytes())
}

fn cond(field_id: &str, op: FilterOperator, value: Value) -> ViewFilter {
    ViewFilter::Condition(FilterCondition {
        field_id: field_id.to_string(),
        op,
        value,
    })
}

/// The same data, written through the public API.
async fn seed(store: &CrmStore) {
    for (name, stage, amount) in [
        ("Small deal", "Lead", 100_00),
        ("medium Deal", "Proposal", 5_000_00),
        ("Large deal", "Proposal", 50_000_00),
        ("Won deal", "Won", 7_00),
        ("Lost deal", "Lost", 3_00),
    ] {
        make_record(
            store,
            "deal",
            &[
                ("name", json!(name)),
                ("stage", json!(stage)),
                ("amount", json!(amount)),
            ],
        )
        .await;
    }
    for (name, domain) in [
        ("Acme", "acme.com"),
        ("ACME Inc", " ACME.com "),
        ("Globex", "globex.com"),
        ("Initech", ""),
    ] {
        make_record(
            store,
            "company",
            &[("name", json!(name)), ("domain", json!(domain))],
        )
        .await;
    }
    for (name, email, notes) in [
        ("Jane Doe", "jane@acme.com", format!("met at {SECRET} expo")),
        ("John Roe", "john@globex.com", "prefers email".to_string()),
    ] {
        make_record(
            store,
            "person",
            &[
                ("name", json!(name)),
                ("email", json!(email)),
                ("notes", json!(notes)),
            ],
        )
        .await;
    }
}

/// Everything a parity run compares, as plain strings.
async fn outcomes(store: &CrmStore) -> Vec<String> {
    let mut out = Vec::new();
    let titles = |page: &RecordPage| {
        format!(
            "{}:{}",
            page.total,
            page.items
                .iter()
                .map(|r| r.title.clone())
                .collect::<Vec<_>>()
                .join("|")
        )
    };
    let queries = [
        RecordQuery {
            object_id: "deal".into(),
            filter: Some(ViewFilter::And {
                filters: vec![
                    cond(
                        FLD_DEAL_STAGE,
                        FilterOperator::IsAnyOf,
                        json!([OPT_DEAL_STAGE_PROPOSAL, OPT_DEAL_STAGE_WON]),
                    ),
                    cond("amount", FilterOperator::Gte, json!(7_00)),
                ],
            }),
            sorts: vec![ViewSort::desc("amount")],
            ..Default::default()
        },
        RecordQuery {
            object_id: "deal".into(),
            filter: Some(cond("name", FilterOperator::Contains, json!("DEAL"))),
            sorts: vec![ViewSort::asc("title")],
            ..Default::default()
        },
        RecordQuery {
            object_id: "deal".into(),
            filter: Some(cond(
                "amount",
                FilterOperator::Between,
                json!([3_00, 5_000_00]),
            )),
            sorts: vec![ViewSort::asc("amount")],
            ..Default::default()
        },
        RecordQuery {
            object_id: "company".into(),
            filter: Some(ViewFilter::Or {
                filters: vec![
                    cond("domain", FilterOperator::IsEmpty, Value::Null),
                    cond("domain", FilterOperator::EndsWith, json!(".com")),
                ],
            }),
            sorts: vec![ViewSort::desc("domain"), ViewSort::asc("name")],
            ..Default::default()
        },
        RecordQuery {
            object_id: "person".into(),
            filter: Some(ViewFilter::Not {
                filter: Box::new(cond("email", FilterOperator::StartsWith, json!("JOHN"))),
            }),
            ..Default::default()
        },
        RecordQuery {
            object_id: "person".into(),
            search: Some("expo".into()),
            ..Default::default()
        },
        RecordQuery {
            object_id: "deal".into(),
            search: Some("large".into()),
            ..Default::default()
        },
    ];
    for query in &queries {
        out.push(titles(&store.query_records(query, 10, 0).await.unwrap()));
        out.push(store.count_records(query).await.unwrap().to_string());
    }
    let report = store
        .pipeline_report(&PipelineRequest {
            object_id: Some("deal".into()),
            include_closed: true,
            ..Default::default()
        })
        .await
        .unwrap()
        .unwrap();
    out.push(format!(
        "pipeline {} {} {} {} {} {} {}",
        report.total_records,
        report.total_value_cents,
        report.unassigned_count,
        report.won_count,
        report.won_value_cents,
        report.lost_count,
        report
            .stages
            .iter()
            .map(|s| format!("{s:?}"))
            .collect::<Vec<_>>()
            .join(";")
    ));
    let board = store
        .run_view(VIEW_DEAL_PIPELINE, &ViewQueryOverrides::default(), 20, 0)
        .await
        .unwrap()
        .unwrap();
    for group in board.groups.unwrap() {
        out.push(format!(
            "board {:?} {} {:?} {}",
            group.option_id,
            group.total,
            group.value_cents,
            {
                // Default board order ties on the creation instant and breaks the
                // tie by random id, so compare the column's members, not order.
                let mut titles: Vec<String> =
                    group.records.iter().map(|r| r.title.clone()).collect();
                titles.sort();
                titles.join("|")
            }
        ));
    }
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
    for candidate in scan.candidates {
        out.push(format!(
            "dup {} {} {}",
            candidate.value,
            candidate.record_ids.len(),
            {
                let mut titles = candidate.titles.clone();
                titles.sort();
                titles.join("|")
            }
        ));
    }
    // Unique email: case and whitespace do not make a new value.
    let clash = store
        .create_record(
            "person",
            &CreateRecordRequest {
                values: bag(&[
                    ("name", json!("Jane Again")),
                    ("email", json!(" JANE@acme.com ")),
                ]),
                created_by: None,
            },
        )
        .await
        .unwrap();
    out.push(format!(
        "unique {:?}",
        clash
            .err()
            .map(|errors| errors.iter().map(|e| e.code).collect::<Vec<_>>())
    ));
    // Import dedupe: an existing email matches regardless of case.
    let job = store
        .create_import(
            &CreateImportRequest {
                object_id: "person".into(),
                filename: Some(format!("{SECRET}.csv")),
                csv: format!("Name,Email\nJane D,JANE@ACME.COM\nNew Person,new@{SECRET}.com\n"),
                delimiter: None,
                has_header: None,
            },
            MAX_TEST_IMPORT,
        )
        .await
        .unwrap()
        .unwrap();
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
    out.push(format!(
        "import {} {} {} {}",
        preview.create_count, preview.update_count, preview.skip_count, preview.error_count
    ));
    let search = store
        .search(
            &SearchQuery {
                query: "acme".into(),
                ..Default::default()
            },
            10,
            0,
        )
        .await
        .unwrap();
    let mut hit_titles: Vec<String> = search.hits.iter().map(|h| h.title.clone()).collect();
    hit_titles.sort();
    out.push(format!("search {} {}", search.total, hit_titles.join("|")));
    out
}

#[tokio::test]
async fn sealed_queries_match_the_plaintext_behaviour() {
    let plain = CrmStore::open_in_memory_with_keys(&PluginKeys::compat()).unwrap();
    assert_eq!(plain.seal_health()["state"], "compat");
    let sealed = CrmStore::open_in_memory_with_keys(&keys(1)).unwrap();
    assert_eq!(sealed.seal_health()["state"], "ok");
    seed(&plain).await;
    seed(&sealed).await;
    let expected = outcomes(&plain).await;
    let actual = outcomes(&sealed).await;
    assert_eq!(actual, expected);
    // The fixture really exercised the operators (not two empty answers).
    assert!(expected
        .iter()
        .any(|line| line.starts_with("dup acme.com 2")));
    assert!(expected.iter().any(|line| line.contains("NotUnique")));
    assert!(expected.iter().any(|line| line == "import 1 0 1 0"));
}

/// List, board and report queries open each candidate row once (title and
/// data), however many filters and sorts they carry, and the page's total comes
/// from the same pass.
#[tokio::test]
async fn record_queries_open_each_row_once() {
    let store = CrmStore::open_in_memory_with_keys(&keys(1)).unwrap();
    seed(&store).await;
    let deals = store
        .count_records(&RecordQuery {
            object_id: "deal".into(),
            ..Default::default()
        })
        .await
        .unwrap() as u64;
    assert_eq!(deals, 5);
    let opens = || {
        store
            .crypto
            .open_calls
            .load(std::sync::atomic::Ordering::Relaxed)
    };

    let before = opens();
    let page = store
        .query_records(
            &RecordQuery {
                object_id: "deal".into(),
                filter: Some(ViewFilter::Or {
                    filters: vec![
                        cond(FLD_DEAL_STAGE, FilterOperator::IsNotEmpty, Value::Null),
                        cond(FLD_DEAL_AMOUNT, FilterOperator::Gt, json!(0)),
                    ],
                }),
                sorts: vec![
                    ViewSort {
                        field_id: FLD_DEAL_AMOUNT.into(),
                        direction: SortDirection::Desc,
                    },
                    ViewSort::asc("title"),
                ],
                ..Default::default()
            },
            2,
            0,
        )
        .await
        .unwrap();
    assert_eq!(page.total, 5);
    assert_eq!(page.items.len(), 2);
    assert_eq!(page.items[0].title, "Large deal");
    assert_eq!(
        opens() - before,
        2 * deals,
        "one title and one data open per row"
    );

    let before = opens();
    let past_the_end = store
        .query_records(
            &RecordQuery {
                object_id: "deal".into(),
                ..Default::default()
            },
            10,
            50,
        )
        .await
        .unwrap();
    assert!(past_the_end.items.is_empty());
    assert_eq!(past_the_end.total, 5);
    assert_eq!(opens() - before, 4 * deals, "the page, then the count");

    let before = opens();
    store
        .pipeline_report(&PipelineRequest {
            object_id: Some("deal".into()),
            include_closed: true,
            ..Default::default()
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(opens() - before, 2 * deals, "every stage from one pass");
}

#[tokio::test]
async fn content_is_sealed_on_disk_and_the_index_holds_no_words() {
    let path = db_path();
    let store = CrmStore::open_with_keys(path.clone(), &keys(2)).unwrap();
    seed(&store).await;
    let person = store
        .query_records(
            &RecordQuery {
                object_id: "person".into(),
                search: Some(SECRET.into()),
                ..Default::default()
            },
            10,
            0,
        )
        .await
        .unwrap();
    assert_eq!(person.total, 1, "search finds the sealed record");
    store
        .create_activity(&CreateActivityRequest {
            record_id: Some(person.items[0].id.clone()),
            kind: ActivityKind::Note,
            title: format!("call {SECRET}"),
            body: Some(format!("body {SECRET}")),
            ..Default::default()
        })
        .await
        .unwrap()
        .unwrap();
    outcomes(&store).await;
    let hits = store
        .search(
            &SearchQuery {
                query: SECRET.into(),
                ..Default::default()
            },
            10,
            0,
        )
        .await
        .unwrap();
    assert_eq!(hits.total, 1);
    assert!(hits.hits[0]
        .snippet
        .contains(&format!("<mark>{SECRET}</mark>")));
    let feed = store
        .query_activities(
            &ActivityQuery {
                search: Some(SECRET.into()),
                ..Default::default()
            },
            10,
            0,
        )
        .await
        .unwrap();
    assert_eq!(feed.total, 1);
    {
        let conn = store.conn.lock().await;
        let cipher = keys(2).content_cipher(crypto::STORE).unwrap();
        ryu_crypto::content_sqlite::assert_all_open(
            &conn,
            &cipher,
            &crypto::column_plans().unwrap(),
        )
        .unwrap();
    }
    assert!(
        !disk_has(&path, SECRET),
        "no plaintext in crm.db or its WAL"
    );
    assert!(!disk_has(&path, "Jane Doe"));
}

#[tokio::test]
async fn a_legacy_plaintext_store_is_migrated_once_and_its_index_rebuilt() {
    let path = db_path();
    {
        // A store from before sealing: plaintext values and the old `records_fts`.
        let store = CrmStore::open_with_keys(path.clone(), &PluginKeys::compat()).unwrap();
        seed(&store).await;
        let conn = store.conn.lock().await;
        conn.execute_batch(
            "CREATE VIRTUAL TABLE records_fts USING fts5(title, body);
             INSERT INTO records_fts(rowid, title, body) SELECT rowid, title, data FROM records;
             DELETE FROM crm_meta;
             PRAGMA user_version = 1;",
        )
        .unwrap();
    }
    assert!(disk_has(&path, SECRET));
    let store = CrmStore::open_with_keys(path.clone(), &keys(3)).unwrap();
    let expected = {
        let plain = CrmStore::open_in_memory_with_keys(&PluginKeys::compat()).unwrap();
        seed(&plain).await;
        outcomes(&plain).await
    };
    let snapshot = |conn: &Connection| -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT title || data FROM records ORDER BY id")
            .unwrap();
        let rows = stmt.query_map([], |r| r.get(0)).unwrap();
        rows.map(Result::unwrap).collect()
    };
    let first = {
        let conn = store.conn.lock().await;
        let fts: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'records_fts'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(fts, 0, "the plaintext index is gone");
        let cipher = keys(3).content_cipher(crypto::STORE).unwrap();
        ryu_crypto::content_sqlite::assert_all_open(
            &conn,
            &cipher,
            &crypto::column_plans().unwrap(),
        )
        .unwrap();
        snapshot(&conn)
    };
    assert!(!disk_has(&path, SECRET), "migration scrubbed the plaintext");
    // Same answers as a plaintext store over the same data (search included).
    assert_eq!(outcomes(&store).await, expected);
    drop(store);
    let again = CrmStore::open_with_keys(path, &keys(3)).unwrap();
    let mut conn = again.conn.lock().await;
    // A second open seals nothing new (the outcomes run wrote fresh rows, which
    // were sealed when written); legacy rows were sealed exactly once.
    let second = snapshot(&conn);
    assert!(first.iter().all(|row| second.contains(row)));
    let report = ryu_crypto::content_sqlite::migrate_columns(
        &mut conn,
        &keys(3).content_cipher(crypto::STORE).unwrap(),
        &crypto::column_plans().unwrap(),
    )
    .unwrap();
    assert_eq!(report.sealed, 0);
}

#[tokio::test]
async fn list_entries_skip_a_record_whose_sealed_value_does_not_open() {
    let store = CrmStore::open_in_memory_with_keys(&keys(4)).unwrap();
    let a = make_record(&store, "company", &[("name", json!("Alpha"))]).await;
    let b = make_record(&store, "company", &[("name", json!("Beta"))]).await;
    let list = store
        .create_list(&CreateListRequest {
            object_id: "company".into(),
            name: "Targets".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    for record in [&a, &b] {
        store
            .add_list_entry(
                &list.id,
                &AddListEntryRequest {
                    record_id: record.id.clone(),
                    values: bag(&[]),
                },
            )
            .await
            .unwrap()
            .unwrap();
    }
    store
        .conn
        .lock()
        .await
        .execute(
            "UPDATE records SET data = (SELECT data FROM records WHERE id = ?1) WHERE id = ?2",
            params![a.id, b.id],
        )
        .unwrap();
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
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].record.title, "Alpha");
}

#[tokio::test]
async fn a_value_copied_to_another_record_does_not_open_and_lists_skip_it() {
    let store = CrmStore::open_in_memory_with_keys(&keys(4)).unwrap();
    let a = make_record(&store, "company", &[("name", json!("Alpha"))]).await;
    let b = make_record(&store, "company", &[("name", json!("Beta"))]).await;
    store
        .conn
        .lock()
        .await
        .execute(
            "UPDATE records SET data = (SELECT data FROM records WHERE id = ?1) WHERE id = ?2",
            params![a.id, b.id],
        )
        .unwrap();
    assert!(store.get_record(&b.id).await.is_err());
    let page = store
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
    assert_eq!(page.total, 1);
    assert_eq!(page.items[0].title, "Alpha");
    assert!(store.seal_health()["unreadable_values"].as_u64().unwrap() >= 1);

    // Index rebuilds skip the row instead of failing on it.
    assert_eq!(store.reindex_all().await.unwrap(), 1);
    let field = store
        .create_field(
            "company",
            None,
            &CreateFieldRequest {
                slug: "motto".into(),
                name: "Motto".into(),
                field_type: FieldType::Text,
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .unwrap();
    assert!(store.delete_field(&field.id).await.unwrap());
    let hits = store
        .search(
            &SearchQuery {
                query: "alpha".into(),
                ..Default::default()
            },
            10,
            0,
        )
        .await
        .unwrap();
    assert_eq!(hits.total, 1);
}

#[tokio::test]
async fn a_value_that_does_not_open_does_not_block_the_open_time_index_rebuild() {
    let path = db_path();
    {
        let store = CrmStore::open_with_keys(path.clone(), &keys(4)).unwrap();
        let a = make_record(&store, "company", &[("name", json!("Alpha"))]).await;
        let b = make_record(&store, "company", &[("name", json!("Beta"))]).await;
        let conn = store.conn.lock().await;
        conn.execute(
            "UPDATE records SET data = (SELECT data FROM records WHERE id = ?1) WHERE id = ?2",
            params![a.id, b.id],
        )
        .unwrap();
        conn.execute("DELETE FROM crm_meta WHERE key = 'terms_index'", [])
            .unwrap();
    }
    let store = CrmStore::open_with_keys(path, &keys(4)).unwrap();
    let hits = store
        .search(
            &SearchQuery {
                query: "alpha".into(),
                ..Default::default()
            },
            10,
            0,
        )
        .await
        .unwrap();
    assert_eq!(hits.total, 1);
    assert!(store.seal_health()["unreadable_values"].as_u64().unwrap() >= 1);
}

#[tokio::test]
async fn temp_storage_stays_in_memory() {
    let store = CrmStore::open_with_keys(db_path(), &keys(4)).unwrap();
    let mode: i64 = store
        .conn
        .lock()
        .await
        .query_row("PRAGMA temp_store", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, 2);
    let memory = CrmStore::open_in_memory_with_keys(&keys(4)).unwrap();
    let mode: i64 = memory
        .conn
        .lock()
        .await
        .query_row("PRAGMA temp_store", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, 2);
}

#[tokio::test]
async fn another_key_or_no_key_refuses_writes_and_keeps_the_data() {
    let path = db_path();
    {
        let store = CrmStore::open_with_keys(path.clone(), &keys(5)).unwrap();
        make_record(&store, "company", &[("name", json!("Kept"))]).await;
    }
    for other in [keys(6), PluginKeys::compat()] {
        let store = CrmStore::open_with_keys(path.clone(), &other).unwrap();
        assert_eq!(store.seal_health()["state"], "unhealthy");
        let refused = store
            .create_record(
                "company",
                &CreateRecordRequest {
                    values: bag(&[("name", json!("New"))]),
                    created_by: None,
                },
            )
            .await;
        assert!(refused.is_err());
    }
    let back = CrmStore::open_with_keys(path, &keys(5)).unwrap();
    let page = back
        .query_records(
            &RecordQuery {
                object_id: "company".into(),
                search: Some("kept".into()),
                ..Default::default()
            },
            10,
            0,
        )
        .await
        .unwrap();
    assert_eq!(page.items[0].title, "Kept");
}

#[tokio::test]
async fn applying_an_import_drops_the_uploaded_csv() {
    let store = CrmStore::open_in_memory_with_keys(&keys(7)).unwrap();
    let job = store
        .create_import(
            &CreateImportRequest {
                object_id: "company".into(),
                filename: None,
                csv: "Name\nOne\nTwo\n".into(),
                delimiter: None,
                has_header: None,
            },
            MAX_TEST_IMPORT,
        )
        .await
        .unwrap()
        .unwrap();
    store.dry_run_import(&job.id).await.unwrap().unwrap();
    store.apply_import(&job.id).await.unwrap().unwrap();
    let raw: String = store
        .conn
        .lock()
        .await
        .query_row(
            "SELECT raw_csv FROM import_jobs WHERE id = ?1",
            params![job.id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(raw.is_empty());
}

#[test]
fn select_lists_match_the_literal_column_constants() {
    assert_eq!(
        select_list(RECORD_COLUMNS, "records", ""),
        squash(COLS_RECORD)
    );
    assert_eq!(
        select_list(ACTIVITY_COLUMNS, "activities", ""),
        squash(COLS_ACTIVITY)
    );
    assert_eq!(
        select_list(LIST_ENTRY_COLUMNS, "list_entries", ""),
        squash(COLS_LIST_ENTRY)
    );
    assert_eq!(
        select_list(IMPORT_COLUMNS, "import_jobs", ""),
        squash(COLS_IMPORT)
    );
}

fn squash(cols: &str) -> String {
    cols.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn snippets_mark_the_matched_words() {
    let snippet = crypto::make_snippet("we met Jane at the Zebra expo in May", "zebra", 12);
    assert!(snippet.contains("<mark>Zebra</mark>"));
}

#[tokio::test]
async fn the_index_holds_no_prefix_tokens_and_an_old_prefix_index_is_rebuilt() {
    let path = db_path();
    let prefix_token = keys(7)
        .blind_index(crypto::STORE, "prefixes")
        .unwrap()
        .term_token("zebr");
    let kcv = keys(7)
        .content_cipher(crypto::STORE)
        .unwrap()
        .kcv()
        .to_owned();
    {
        let store = CrmStore::open_with_keys(path.clone(), &keys(7)).unwrap();
        seed(&store).await;
        let conn = store.conn.lock().await;
        let matches = |token: &str| -> i64 {
            conn.query_row(
                "SELECT COUNT(*) FROM records_terms WHERE records_terms MATCH ?1",
                params![format!("\"{token}\"")],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(matches(&prefix_token), 0, "no prefix token is written");
        // Simulate an index built by the earlier build: tagged with the bare key
        // check value and holding prefix tokens.
        conn.execute(
            "INSERT INTO records_terms(rowid, terms) VALUES (999999, ?1)",
            params![prefix_token],
        )
        .unwrap();
        conn.execute(
            "UPDATE crm_meta SET value = ?1 WHERE key = 'terms_index'",
            params![kcv],
        )
        .unwrap();
        assert_eq!(matches(&prefix_token), 1);
    }
    let store = CrmStore::open_with_keys(path.clone(), &keys(7)).unwrap();
    let conn = store.conn.lock().await;
    let leftover: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM records_terms WHERE records_terms MATCH ?1",
            params![format!("\"{prefix_token}\"")],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(leftover, 0, "the old prefix index was dropped and rebuilt");
    let tag: String = conn
        .query_row(
            "SELECT value FROM crm_meta WHERE key = 'terms_index'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tag, format!("{kcv}:v2"));
    drop(conn);
    // Whole words still find the record.
    let hits = store
        .search(
            &SearchQuery {
                query: SECRET.into(),
                ..Default::default()
            },
            10,
            0,
        )
        .await
        .unwrap();
    assert_eq!(hits.total, 1);
    assert!(!disk_has(&path, &prefix_token));
}
