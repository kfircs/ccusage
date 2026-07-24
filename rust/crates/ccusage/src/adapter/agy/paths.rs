use std::{collections::HashSet, env, path::PathBuf};

use crate::Result;

pub(super) const AGY_DATA_DIR_ENV: &str = "AGY_DATA_DIR";
pub(super) const AGY_CONVERSATIONS_DIR_NAME: &str = "conversations";
pub(super) const AGY_SUMMARIES_DB_FILE_NAME: &str = "conversation_summaries.db";

pub(super) fn paths() -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    let mut seen = HashSet::new();
    if let Ok(env_paths) = env::var(AGY_DATA_DIR_ENV) {
        for raw in env_paths
            .split(',')
            .map(str::trim)
            .filter(|path| !path.is_empty())
        {
            let path = PathBuf::from(raw);
            if path.is_dir() && seen.insert(path.clone()) {
                paths.push(path);
            }
        }
        return Ok(paths);
    }

    if let Some(home) = crate::home::home_dir() {
        let path = home.join(".gemini").join("antigravity-cli");
        if path.is_dir() && seen.insert(path.clone()) {
            paths.push(path);
        }
    }
    Ok(paths)
}

pub(super) fn db_paths() -> Result<Vec<PathBuf>> {
    let mut db_paths = Vec::new();
    let mut seen = HashSet::new();
    for data_dir in paths()? {
        let conversations_dir = data_dir.join(AGY_CONVERSATIONS_DIR_NAME);
        let Ok(entries) = std::fs::read_dir(&conversations_dir) else {
            continue;
        };
        for entry in entries.filter_map(std::result::Result::ok) {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_file() {
                continue;
            }
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("db") {
                continue;
            }
            if path.file_name().and_then(|name| name.to_str()) == Some(AGY_SUMMARIES_DB_FILE_NAME) {
                continue;
            }
            if seen.insert(path.clone()) {
                db_paths.push(path);
            }
        }
    }
    // `seen` already dedups on insert; sort only orders the result.
    db_paths.sort();
    Ok(db_paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ccusage_test_support::{EnvVarGuard, EnvVarsGuard, Fixture, fs_fixture};

    #[test]
    fn discovers_db_files_via_env_var_override() {
        let fixture = fs_fixture!({
            "conversations/test.db": "",
        });
        let _cleanup = EnvVarGuard::set(AGY_DATA_DIR_ENV, fixture.root());

        let paths = db_paths().unwrap();

        assert_eq!(paths, vec![fixture.path("conversations/test.db")]);
    }

    #[test]
    fn returns_ok_when_env_var_is_not_set_and_home_dir_is_unavailable() {
        let mut vars = vec![
            ("HOME", Some(std::ffi::OsString::new())),
            ("USERPROFILE", Some(std::ffi::OsString::new())),
            ("HOMEDRIVE", Some(std::ffi::OsString::new())),
            ("HOMEPATH", Some(std::ffi::OsString::new())),
        ];
        if env::var_os(AGY_DATA_DIR_ENV).is_some() {
            vars.push((AGY_DATA_DIR_ENV, Some(std::ffi::OsString::new())));
        }
        let _guard = EnvVarsGuard::set_many(vars);

        let paths = db_paths().unwrap();

        assert!(paths.is_empty());
    }

    #[test]
    fn discovers_db_files_from_multiple_env_var_paths_and_dedups() {
        let first = fs_fixture!({
            "conversations/first.db": "",
        });
        let second = fs_fixture!({
            "conversations/second.db": "",
        });
        let _cleanup = EnvVarGuard::set(
            AGY_DATA_DIR_ENV,
            format!("{}, {}", first.root().display(), second.root().display()),
        );

        let mut paths = db_paths().unwrap();
        let mut expected = vec![
            first.path("conversations/first.db"),
            second.path("conversations/second.db"),
        ];
        paths.sort();
        expected.sort();

        assert_eq!(paths, expected);
    }

    #[test]
    fn skips_old_pb_files() {
        let fixture = fs_fixture!({
            "conversations/test.db": "",
            "conversations/old.pb": "",
        });
        let _cleanup = EnvVarGuard::set(AGY_DATA_DIR_ENV, fixture.root());

        let paths = db_paths().unwrap();

        assert_eq!(paths, vec![fixture.path("conversations/test.db")]);
    }

    #[test]
    fn skips_conversation_summaries_metadata_db() {
        let fixture = fs_fixture!({
            "conversations/abc.db": "",
            "conversation_summaries.db": "",
        });
        let _cleanup = EnvVarGuard::set(AGY_DATA_DIR_ENV, fixture.root());

        let paths = db_paths().unwrap();

        assert_eq!(paths, vec![fixture.path("conversations/abc.db")]);
    }

    #[test]
    fn silently_skips_nonexistent_env_var_directories() {
        let _cleanup = EnvVarGuard::set(AGY_DATA_DIR_ENV, "/nonexistent/path");

        let paths = db_paths().unwrap();

        assert!(paths.is_empty());
    }

    #[test]
    fn returns_empty_vec_for_empty_conversations_dir() {
        let fixture = Fixture::new();
        let _ = fixture.create_dir_all("conversations");
        let _cleanup = EnvVarGuard::set(AGY_DATA_DIR_ENV, fixture.root());

        let paths = db_paths().unwrap();

        assert!(paths.is_empty());
    }
}
