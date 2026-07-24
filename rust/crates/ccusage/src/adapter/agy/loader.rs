use std::{collections::HashSet, path::PathBuf};

use crate::{LoadedEntry, PricingMap, Result, cli::SharedArgs, parse_tz, read_files_parallel};

pub(crate) fn load_entries(shared: &SharedArgs, pricing: &PricingMap) -> Result<Vec<LoadedEntry>> {
    crate::progress::track_usage_load(crate::progress::UsageLoadAgent::Agy, shared.json, || {
        load_entries_inner(shared, pricing)
    })
}

fn load_entries_inner(shared: &SharedArgs, pricing: &PricingMap) -> Result<Vec<LoadedEntry>> {
    let tz = parse_tz(shared.timezone.as_deref());
    let db_paths: Vec<PathBuf> = super::paths::db_paths()?;
    // Load each conversation database in parallel (a fresh read-only
    // connection per DB), then run the sequential id dedup over the
    // original path order so the surviving record per id matches the
    // single-threaded read.
    let loaded = read_files_parallel(&db_paths, shared.single_thread, |db_path| {
        super::parser::parse_conversation_db(db_path, tz.as_ref(), shared.mode, pricing)
    });
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    for db_entries in loaded {
        for entry in db_entries {
            if let Some(id) = entry.data.message.id.as_deref()
                && !seen.insert(id.to_string())
            {
                continue;
            }
            entries.push(entry);
        }
    }
    entries.sort_by_key(|entry| entry.timestamp);
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::super::support::*;
    use super::*;
    use crate::{PricingMap, cli::CostMode};
    use ccusage_test_support::{EnvVarGuard, EnvVarsGuard, fs_fixture};

    /// Test: Loads entries from a `.db` file in the conversations
    /// directory. Build a single conversation db with a model-response
    /// step and verify the loader surfaces it with the correct model
    /// and token values.
    #[test]
    fn loads_entries_from_conversation_db() {
        let fixture = fs_fixture!({});
        let db_path = fixture.path("conversations/test-conv.db");
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let db = open_db(&db_path);
        create_schema(&db);
        insert_gen_metadata(&db, 0, &build_gen_metadata_row(1035, "claude-sonnet-4-6"));
        let payload = build_step_payload(1_767_312_000, 0, 1035, 797, 169, 22_472, Some("req-1"));
        insert_step(&db, 1, STEP_TYPE_MODEL_RESPONSE, &payload);

        let _cleanup = EnvVarGuard::set("AGY_DATA_DIR", fixture.root());
        let shared = SharedArgs {
            mode: CostMode::Display,
            timezone: Some("UTC".to_string()),
            ..SharedArgs::default()
        };
        let entries = load_entries(&shared, &PricingMap::load_embedded()).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(entries[0].data.message.usage.input_tokens, 22_472);
        assert_eq!(entries[0].data.message.usage.output_tokens, 797);
        assert_eq!(entries[0].extra_total_tokens, 169);
        assert_eq!(entries[0].data.message.id.as_deref(), Some("req-1"));
    }

    /// Test: Returns an empty result when no data dir is set and home
    /// is unavailable. This mirrors the paths.rs test pattern — clearing
    /// HOME/USERPROFILE and AGY_DATA_DIR so `db_paths()` finds nothing.
    #[test]
    fn returns_empty_when_no_data_dir_and_home_unavailable() {
        let mut vars = vec![
            ("HOME", Some(std::ffi::OsString::new())),
            ("USERPROFILE", Some(std::ffi::OsString::new())),
            ("HOMEDRIVE", Some(std::ffi::OsString::new())),
            ("HOMEPATH", Some(std::ffi::OsString::new())),
        ];
        if std::env::var_os("AGY_DATA_DIR").is_some() {
            vars.push(("AGY_DATA_DIR", Some(std::ffi::OsString::new())));
        }
        let _guard = EnvVarsGuard::set_many(vars);

        let shared = SharedArgs {
            mode: CostMode::Display,
            timezone: Some("UTC".to_string()),
            ..SharedArgs::default()
        };
        let entries = load_entries(&shared, &PricingMap::load_embedded()).unwrap();

        assert!(entries.is_empty());
    }

    /// Test: Sorts entries by timestamp. Insert two steps with
    /// different timestamps (out of order in the db) and verify the
    /// loader returns them sorted ascending by timestamp.
    #[test]
    fn sorts_entries_by_timestamp() {
        let fixture = fs_fixture!({});
        let db_path = fixture.path("conversations/test-sort.db");
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let db = open_db(&db_path);
        create_schema(&db);
        insert_gen_metadata(&db, 0, &build_gen_metadata_row(1035, "claude-sonnet-4-6"));

        // Insert the later-timestamp step first (lower idx but later time).
        let later = build_step_payload(1_767_312_100, 0, 1035, 50, 0, 1_000, Some("req-later"));
        insert_step(&db, 1, STEP_TYPE_MODEL_RESPONSE, &later);
        // Insert the earlier-timestamp step second (higher idx but earlier time).
        let earlier = build_step_payload(1_767_312_000, 0, 1035, 100, 0, 500, Some("req-earlier"));
        insert_step(&db, 2, STEP_TYPE_MODEL_RESPONSE, &earlier);

        let _cleanup = EnvVarGuard::set("AGY_DATA_DIR", fixture.root());
        let shared = SharedArgs {
            mode: CostMode::Display,
            timezone: Some("UTC".to_string()),
            ..SharedArgs::default()
        };
        let entries = load_entries(&shared, &PricingMap::load_embedded()).unwrap();

        assert_eq!(entries.len(), 2);
        // The earlier-timestamp entry must come first.
        assert_eq!(entries[0].timestamp.as_millis(), 1_767_312_000_000);
        assert_eq!(entries[1].timestamp.as_millis(), 1_767_312_100_000);
    }
}
