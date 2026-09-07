use super::*;

const MAX_TEST_IMPORT: usize = 1024 * 1024;

async fn store() -> CrmStore {
    CrmStore::open_in_memory().expect("in-memory store")
}

fn bag(pairs: &[(&str, Value)]) -> ValueBag {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), v.clone()))
        .collect()
}

async fn make_record(store: &CrmStore, object: &str, values: &[(&str, Value)]) -> Record {
    store
        .create_record(
            object,
            &CreateRecordRequest {
                values: bag(values),
                created_by: None,
            },
        )
        .await
        .expect("no infrastructure error")
        .expect("values accepted")
}

mod core;
mod imports;
mod records;
mod views;
