use serde_json::{Value, json};

use crate::{
    BucketKind, LoadedEntry, Result, SessionAccumulator,
    adapter::opencode,
    cli::{AgentReportKind, WeekDay},
    summarize_by_key, summarize_summaries_by_bucket, totals_json,
};

pub(crate) fn report_from_rows(rows: &[crate::UsageSummary], kind: AgentReportKind) -> Value {
    let rows_json = rows
        .iter()
        .map(|row| opencode::agent_summary_json(row, kind, kind == AgentReportKind::Session))
        .collect::<Vec<_>>();
    json!({
        rows_key(kind): rows_json,
        "totals": totals_json(rows),
    })
}

pub(crate) fn summarize_entries(
    entries: &[LoadedEntry],
    kind: AgentReportKind,
) -> Result<Vec<crate::UsageSummary>> {
    match kind {
        AgentReportKind::Daily => summarize_by_key(
            entries,
            |entry| entry.date.clone(),
            |date| (date.to_string(), None),
        ),
        AgentReportKind::Monthly => {
            let daily = summarize_entries(entries, AgentReportKind::Daily)?;
            Ok(summarize_summaries_by_bucket(
                &daily,
                BucketKind::Monthly,
                WeekDay::Sunday,
            ))
        }
        AgentReportKind::Session => {
            let mut groups = std::collections::BTreeMap::<String, SessionAccumulator>::new();
            for entry in entries {
                groups
                    .entry(entry.session_id.to_string())
                    .or_default()
                    .add_entry(entry);
            }
            groups
                .into_values()
                .map(|group| group.into_summary())
                .collect()
        }
        AgentReportKind::Weekly => {
            let daily = summarize_entries(entries, AgentReportKind::Daily)?;
            Ok(summarize_summaries_by_bucket(
                &daily,
                BucketKind::Weekly,
                WeekDay::Sunday,
            ))
        }
    }
}

fn rows_key(kind: AgentReportKind) -> &'static str {
    match kind {
        AgentReportKind::Daily => "daily",
        AgentReportKind::Weekly => "weekly",
        AgentReportKind::Monthly => "monthly",
        AgentReportKind::Session => "sessions",
    }
}

#[cfg(test)]
mod tests {
    use super::super::load_entries;
    use super::super::support::*;
    use super::*;
    use crate::{PricingMap, cli::CostMode, cli::SharedArgs};
    use ccusage_test_support::{EnvVarGuard, fs_fixture};

    /// Build a single conversation database with one model-response step
    /// and return a shared args configured for UTC display mode.
    fn build_fixture_db() -> (ccusage_test_support::Fixture, SharedArgs) {
        let fixture = fs_fixture!({});
        let db_path = fixture.path("conversations/test-conv.db");
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let db = open_db(&db_path);
        create_schema(&db);
        insert_gen_metadata(&db, 0, &build_gen_metadata_row(1035, "claude-sonnet-4-6"));
        let payload = build_step_payload(1_767_312_000, 0, 1035, 797, 169, 22_472, Some("req-1"));
        insert_step(&db, 1, STEP_TYPE_MODEL_RESPONSE, &payload);

        let shared = SharedArgs {
            mode: CostMode::Display,
            timezone: Some("UTC".to_string()),
            ..SharedArgs::default()
        };
        (fixture, shared)
    }

    /// Test: load_entries -> summarize_entries -> report_from_rows produces
    /// a daily JSON report with the expected shape and token totals.
    #[test]
    fn daily_report_from_conversation_db_fixture() {
        let (fixture, shared) = build_fixture_db();
        let _cleanup = EnvVarGuard::set("AGY_DATA_DIR", fixture.root());

        let entries = load_entries(&shared, &PricingMap::load_embedded()).unwrap();
        assert_eq!(entries.len(), 1);

        let rows = summarize_entries(&entries, AgentReportKind::Daily).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].date.as_deref(), Some("2026-01-02"));
        assert_eq!(rows[0].input_tokens, 22_472);
        assert_eq!(rows[0].output_tokens, 797);
        assert_eq!(rows[0].extra_total_tokens, 169);
        assert_eq!(rows[0].total_tokens(), 23_438);
        assert_eq!(rows[0].models_used, vec!["claude-sonnet-4-6"]);

        let report = report_from_rows(&rows, AgentReportKind::Daily);
        let daily = report.get("daily").and_then(Value::as_array).unwrap();
        assert_eq!(daily.len(), 1);
        let day0 = &daily[0];
        assert_eq!(day0.get("date").and_then(Value::as_str), Some("2026-01-02"));
        assert_eq!(
            day0.get("inputTokens").and_then(Value::as_u64),
            Some(22_472)
        );
        assert_eq!(day0.get("outputTokens").and_then(Value::as_u64), Some(797));
        assert_eq!(
            day0.get("totalTokens").and_then(Value::as_u64),
            Some(23_438)
        );
        let models = day0.get("modelsUsed").and_then(Value::as_array).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].as_str(), Some("claude-sonnet-4-6"));

        let totals = report.get("totals").unwrap();
        assert_eq!(
            totals.get("totalTokens").and_then(Value::as_u64),
            Some(23_438)
        );
        assert_eq!(
            totals.get("inputTokens").and_then(Value::as_u64),
            Some(22_472)
        );
        assert_eq!(
            totals.get("outputTokens").and_then(Value::as_u64),
            Some(797)
        );
    }

    /// Test: the session report shape groups by session id and exposes the
    /// session metadata fields (projectPath, firstActivity, lastActivity).
    #[test]
    fn session_report_from_conversation_db_fixture() {
        let (fixture, shared) = build_fixture_db();
        let _cleanup = EnvVarGuard::set("AGY_DATA_DIR", fixture.root());

        let entries = load_entries(&shared, &PricingMap::load_embedded()).unwrap();
        let rows = summarize_entries(&entries, AgentReportKind::Session).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].session_id.is_some());

        let report = report_from_rows(&rows, AgentReportKind::Session);
        let sessions = report.get("sessions").and_then(Value::as_array).unwrap();
        assert_eq!(sessions.len(), 1);
        let session0 = &sessions[0];
        assert_eq!(
            session0.get("inputTokens").and_then(Value::as_u64),
            Some(22_472)
        );
        assert_eq!(
            session0.get("outputTokens").and_then(Value::as_u64),
            Some(797)
        );
        assert_eq!(
            session0.get("totalTokens").and_then(Value::as_u64),
            Some(23_438)
        );
        // Session metadata is included for session reports.
        assert!(session0.get("projectPath").is_some());
        assert!(session0.get("firstActivity").is_some());
        assert!(session0.get("lastActivity").is_some());

        let totals = report.get("totals").unwrap();
        assert_eq!(
            totals.get("totalTokens").and_then(Value::as_u64),
            Some(23_438)
        );
    }

    /// Test: report_from_rows over an empty entry set yields empty rows and
    /// zeroed totals without panicking.
    #[test]
    fn empty_report_has_zeroed_totals() {
        let rows: Vec<crate::UsageSummary> = Vec::new();
        let report = report_from_rows(&rows, AgentReportKind::Daily);
        let daily = report.get("daily").and_then(Value::as_array).unwrap();
        assert!(daily.is_empty());
        let totals = report.get("totals").unwrap();
        assert_eq!(totals.get("totalTokens").and_then(Value::as_u64), Some(0));
    }
}
