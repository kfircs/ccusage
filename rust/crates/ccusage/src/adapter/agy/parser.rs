use std::{collections::HashMap, path::Path, sync::Arc};

use jiff::tz::TimeZone as JiffTimeZone;

use super::protobuf::{extract_length_delimited, extract_varint};
use crate::{
    LoadedEntry, PricingMap, TimestampMs, TokenUsageRaw, UsageEntry, UsageMessage,
    calculate_cost_for_usage, cli::CostMode, format_date_tz, format_rfc3339_millis,
    missing_pricing_model_for_usage,
};

/// Step types we care about. The agy database uses integer step type values
/// in the `steps` table; `15` represents a model response and `23` represents
/// a checkpoint that may also carry token usage data.
pub(super) const STEP_TYPE_MODEL_RESPONSE: i64 = 15;
pub(super) const STEP_TYPE_CHECKPOINT: i64 = 23;

/// Protobuf field numbers used inside `step_payload`. The outer
/// `step_payload` is a top-level message whose `f5` is a length-delimited
/// "event" sub-message holding the timestamp and token usage. Inside `f5`:
///   - `f1` is a *nested* length-delimited sub-message carrying the request
///     timestamp as `{f1 = unix seconds, f2 = nanoseconds}`.
///   - `f9` is a length-delimited sub-message holding the usage values.
pub(super) const FIELD_OUTER_EVENT_SUBMESSAGE: u64 = 5;
pub(super) const FIELD_F5_TIMESTAMP_SUBMESSAGE: u64 = 1;
pub(super) const FIELD_TS_SECONDS: u64 = 1;
pub(super) const FIELD_TS_NANOS: u64 = 2;
pub(super) const FIELD_F5_USAGE_SUBMESSAGE: u64 = 9;
pub(super) const FIELD_F9_MODEL_ENUM: u64 = 1;
pub(super) const FIELD_F9_OUTPUT_TOKENS: u64 = 2;
pub(super) const FIELD_F9_THINKING_TOKENS: u64 = 3;
pub(super) const FIELD_F9_CUMULATIVE_INPUT: u64 = 5;
pub(super) const FIELD_F9_REQUEST_ID: u64 = 11;

/// Protobuf field numbers used inside `gen_metadata.data`. Each row's `data`
/// blob is a per-step generation-metadata record; its `f1` sub-message carries
/// the model identity — `f3` is the integer model enum id and `f19` is the
/// human-readable model id string. The enum id is what `step_payload.f5.f9.f1`
/// references, so this builds the per-conversation enum → name mapping.
pub(super) const FIELD_GEN_METADATA_INNER: u64 = 1;
pub(super) const FIELD_GEN_METADATA_ENUM: u64 = 3;
pub(super) const FIELD_GEN_METADATA_MODEL_NAME: u64 = 19;

/// Parse a single agy conversation `.db` file and return `LoadedEntry`
/// values for every step that carries token usage data. The function is
/// defensive — a missing table, unreadable blob, or unknown model enum
/// id never panics and never propagates errors. The caller gets an empty
/// vector when the file is unusable and silently skips individual bad
/// rows when the rest of the file is still readable.
pub(super) fn parse_conversation_db(
    db_path: &Path,
    tz: Option<&JiffTimeZone>,
    mode: CostMode,
    pricing: &PricingMap,
) -> Vec<LoadedEntry> {
    let Ok(connection) =
        sqlite::Connection::open_with_flags(db_path, sqlite::OpenFlags::new().with_read_only())
    else {
        return Vec::new();
    };
    let model_map = read_model_mapping(&connection);
    // Checkpoint/compaction steps (step_type 23) carry an internal model enum
    // (e.g. 1050) that is never named in `gen_metadata`, so they cannot be
    // resolved on their own. Attribute them to the conversation's primary
    // model — the most common enum among the model-response (step_type 15)
    // steps — which `model_for_enum` uses as a fallback for unmapped enums.
    let primary_enum = primary_model_enum(&connection);
    let Ok(mut statement) =
        connection.prepare("SELECT idx, step_type, step_payload FROM steps ORDER BY idx")
    else {
        return Vec::new();
    };
    let session_id = conversation_id_from_path(db_path);
    let project = session_id.clone();
    let project_path = Arc::from(db_path.to_string_lossy().as_ref());
    let mut entries: Vec<LoadedEntry> = Vec::new();
    let mut previous_cumulative_input: Option<u64> = None;
    loop {
        match statement.next() {
            Ok(sqlite::State::Row) => {
                let Some((idx, step_type, step_payload)) = read_step_row(&statement) else {
                    continue;
                };
                if !is_supported_step_type(step_type) {
                    continue;
                }
                let Some(usage_data) = decode_step_usage(&step_payload) else {
                    continue;
                };
                let per_turn_input = per_turn_input_tokens(
                    usage_data.cumulative_input,
                    &mut previous_cumulative_input,
                );
                if let Some(entry) = build_loaded_entry(
                    idx,
                    usage_data,
                    per_turn_input,
                    &session_id,
                    &project,
                    &project_path,
                    tz,
                    mode,
                    pricing,
                    &model_map,
                    primary_enum,
                ) {
                    entries.push(entry);
                }
            }
            Ok(sqlite::State::Done) => break,
            Err(_) => break,
        }
    }
    entries
}

/// Extract the conversation id (UUID-shaped filename) from a `.db` file
/// path. The conversation id is the file stem — the filename without its
/// extension. An empty or unparseable file stem falls back to the full
/// file name so downstream code always sees a non-empty session id.
fn conversation_id_from_path(db_path: &Path) -> Arc<str> {
    let stem = db_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_else(|| {
            db_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("agy-conversation")
        });
    Arc::from(stem)
}

fn is_supported_step_type(step_type: i64) -> bool {
    step_type == STEP_TYPE_MODEL_RESPONSE || step_type == STEP_TYPE_CHECKPOINT
}

/// Read a single row from the `steps` table. Returns `(idx, step_type,
/// step_payload)` when the row is well-formed. Returns `None` if any of
/// the columns fail to decode so the caller can skip the row without
/// losing the rest of the table.
fn read_step_row(statement: &sqlite::Statement<'_>) -> Option<(i64, i64, Vec<u8>)> {
    let idx: i64 = statement.read(0).ok()?;
    let step_type: i64 = statement.read(1).ok()?;
    let step_payload: Vec<u8> = statement.read(2).ok()?;
    Some((idx, step_type, step_payload))
}

/// Read just the model enum (`f9.f1`) from a `step_payload` blob, without
/// decoding the rest. Used by `primary_model_enum` to cheaply tally the
/// model-response enums without paying for a full `decode_step_usage`.
fn decode_model_enum(payload: &[u8]) -> Option<u64> {
    let f5 = extract_length_delimited(payload, FIELD_OUTER_EVENT_SUBMESSAGE)?;
    let f9 = extract_length_delimited(f5, FIELD_F5_USAGE_SUBMESSAGE)?;
    extract_varint(f9, FIELD_F9_MODEL_ENUM)
}

/// Determine the conversation's primary model enum: the most frequently used
/// enum among the model-response (`step_type` 15) steps. Checkpoint steps
/// carry an unnamed internal enum, so this is the best available signal for
/// attributing their usage. Returns `None` when there are no decodable
/// model-response steps.
fn primary_model_enum(connection: &sqlite::Connection) -> Option<u64> {
    let Ok(mut statement) =
        connection.prepare("SELECT step_payload FROM steps WHERE step_type = 15")
    else {
        return None;
    };
    let mut counts: HashMap<u64, usize> = HashMap::new();
    loop {
        match statement.next() {
            Ok(sqlite::State::Row) => {
                let Ok(payload): Result<Vec<u8>, _> = statement.read(0) else {
                    continue;
                };
                if let Some(model_enum) = decode_model_enum(&payload) {
                    *counts.entry(model_enum).or_default() += 1;
                }
            }
            Ok(sqlite::State::Done) => break,
            Err(_) => break,
        }
    }
    counts
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(model_enum, _)| model_enum)
}

/// Decoded usage data extracted from one `step_payload` blob.
struct StepUsage {
    model_enum: u64,
    output_tokens: u64,
    thinking_tokens: u64,
    cumulative_input: u64,
    request_id: Option<String>,
    timestamp_seconds: i64,
    timestamp_nanos: i64,
}

fn decode_step_usage(payload: &[u8]) -> Option<StepUsage> {
    let f5 = extract_length_delimited(payload, FIELD_OUTER_EVENT_SUBMESSAGE)?;
    let f9 = extract_length_delimited(f5, FIELD_F5_USAGE_SUBMESSAGE)?;
    let model_enum = extract_varint(f9, FIELD_F9_MODEL_ENUM)?;
    let output_tokens = extract_varint(f9, FIELD_F9_OUTPUT_TOKENS).unwrap_or(0);
    let thinking_tokens = extract_varint(f9, FIELD_F9_THINKING_TOKENS).unwrap_or(0);
    let cumulative_input = extract_varint(f9, FIELD_F9_CUMULATIVE_INPUT).unwrap_or(0);
    let request_id = extract_length_delimited(f9, FIELD_F9_REQUEST_ID)
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .map(str::to_string);
    // The timestamp lives in a nested sub-message at `f5.f1`:
    // `{f1 = unix seconds, f2 = nanoseconds}`. The earlier decoder read `f5.f1`
    // as a flat varint, which returned the sub-message's leading tag byte
    // (e.g. `8`) and produced 1970-01-01 dates; descend one level instead.
    let (timestamp_seconds, timestamp_nanos) =
        extract_length_delimited(f5, FIELD_F5_TIMESTAMP_SUBMESSAGE)
            .map(|ts| {
                let seconds = extract_varint(ts, FIELD_TS_SECONDS).unwrap_or(0) as i64;
                let nanos = extract_varint(ts, FIELD_TS_NANOS).unwrap_or(0) as i64;
                (seconds, nanos)
            })
            .unwrap_or((0, 0));
    Some(StepUsage {
        model_enum,
        output_tokens,
        thinking_tokens,
        cumulative_input,
        request_id,
        timestamp_seconds,
        timestamp_nanos,
    })
}

/// Convert a cumulative input token count into the per-turn delta. The
/// first observation in a conversation is treated as the entire turn's
/// input (there is no prior baseline to subtract from). Subsequent
/// observations subtract the previously-seen cumulative value and saturate
/// at zero so out-of-order rows can never produce a negative delta.
fn per_turn_input_tokens(cumulative_input: u64, previous: &mut Option<u64>) -> u64 {
    match *previous {
        None => {
            *previous = Some(cumulative_input);
            cumulative_input
        }
        Some(prior) => {
            let delta = cumulative_input.saturating_sub(prior);
            *previous = Some(cumulative_input);
            delta
        }
    }
}

/// Read every `gen_metadata` row and build the enum id → model name
/// mapping for this conversation. Each row's `data` blob is a per-step
/// generation-metadata record; the model identity lives in its `f1`
/// sub-message — `f3` is the integer enum id and `f19` is the model id
/// string. Returns an empty map when the table is missing or unreadable;
/// callers must be able to handle that.
fn read_model_mapping(connection: &sqlite::Connection) -> HashMap<u64, String> {
    let Ok(mut statement) = connection.prepare("SELECT data FROM gen_metadata ORDER BY idx") else {
        return HashMap::new();
    };
    let mut mapping: HashMap<u64, String> = HashMap::new();
    loop {
        match statement.next() {
            Ok(sqlite::State::Row) => {
                let Ok(data) = statement.read::<Vec<u8>, _>(0) else {
                    continue;
                };
                let Some(inner) = extract_length_delimited(&data, FIELD_GEN_METADATA_INNER) else {
                    continue;
                };
                let Some(enum_id) = extract_varint(inner, FIELD_GEN_METADATA_ENUM) else {
                    continue;
                };
                let Some(name_bytes) =
                    extract_length_delimited(inner, FIELD_GEN_METADATA_MODEL_NAME)
                else {
                    continue;
                };
                let Ok(name) = std::str::from_utf8(name_bytes) else {
                    continue;
                };
                let trimmed = name.trim();
                if !trimmed.is_empty() {
                    mapping.insert(enum_id, trimmed.to_string());
                }
            }
            Ok(sqlite::State::Done) => break,
            Err(_) => break,
        }
    }
    mapping
}

/// Build the final `LoadedEntry` for one step. The model is resolved from the
/// per-conversation `gen_metadata` mapping. Unmapped enums — chiefly the
/// checkpoint/compaction enum (1050) that `gen_metadata` never names — fall
/// back to the conversation's primary model (`primary_enum`) so their usage is
/// attributed to a real, priced model instead of a bare number. When no
/// fallback is available the integer id is used as a string so the row still
/// surfaces a model label.
/// Build the `TokenUsageRaw` used for cost calculation.
///
/// Antigravity reports "thinking"/reasoning tokens separately from output
/// tokens, but the upstream pricing model bills them at the output rate.
/// Fold `extra_total_tokens` (the step's thinking-token count) into
/// `output_tokens` before costing so thinking usage is not free. Arithmetic
/// is `saturating_add` to stay panic-free on pathological counts.
fn usage_for_cost(usage: TokenUsageRaw, extra_total_tokens: u64) -> TokenUsageRaw {
    TokenUsageRaw {
        output_tokens: usage.output_tokens.saturating_add(extra_total_tokens),
        cache_creation: None,
        ..usage
    }
}

#[allow(clippy::too_many_arguments)]
fn build_loaded_entry(
    idx: i64,
    usage_data: StepUsage,
    per_turn_input: u64,
    session_id: &Arc<str>,
    project: &Arc<str>,
    project_path: &Arc<str>,
    tz: Option<&JiffTimeZone>,
    mode: CostMode,
    pricing: &PricingMap,
    model_map: &HashMap<u64, String>,
    primary_enum: Option<u64>,
) -> Option<LoadedEntry> {
    let model = model_for_enum(usage_data.model_enum, model_map, primary_enum);
    let timestamp = timestamp_from_step(&usage_data)?;
    let usage = TokenUsageRaw {
        input_tokens: per_turn_input,
        output_tokens: usage_data.output_tokens,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
        speed: None,
        cache_creation: None,
    };
    let extra_total_tokens = usage_data.thinking_tokens;
    let message_id = request_id_for_entry(usage_data.request_id.as_deref(), idx);
    let timestamp_text = format_rfc3339_millis(timestamp);
    let date = format_date_tz(timestamp, tz);
    let data = UsageEntry {
        session_id: Some(session_id.to_string()),
        timestamp: timestamp_text,
        version: None,
        message: UsageMessage {
            usage,
            model: Some(model.clone()),
            id: Some(message_id),
        },
        cost_usd: None,
        request_id: usage_data.request_id.clone(),
        is_api_error_message: None,
        is_sidechain: None,
    };
    let cost_data = UsageEntry {
        message: UsageMessage {
            usage: usage_for_cost(data.message.usage, extra_total_tokens),
            ..data.message.clone()
        },
        ..data.clone()
    };
    let cost = calculate_cost_for_usage(
        cost_data.message.model.as_deref(),
        cost_data.message.usage,
        cost_data.cost_usd,
        mode,
        Some(pricing),
    );
    let missing_pricing_model = missing_pricing_model_for_usage(
        cost_data.message.model.as_deref(),
        cost_data.message.usage,
        cost_data.cost_usd,
        mode,
        Some(pricing),
    );
    Some(LoadedEntry {
        data,
        timestamp,
        date,
        project: Arc::clone(project),
        session_id: Arc::clone(session_id),
        project_path: Arc::clone(project_path),
        cost,
        extra_total_tokens,
        credits: None,
        message_count: None,
        model: Some(model),
        usage_limit_reset_time: None,
        missing_pricing_model,
    })
}

/// Resolve a model enum id to its human-readable model id. The
/// conversation-level `gen_metadata` mapping is the source of truth. When the
/// conversation does not know about the enum — chiefly the checkpoint/
/// compaction enum (1050), which `gen_metadata` never names — attribute the
/// usage to the conversation's primary model (`primary_enum`) so it lands on a
/// real, priced model. When that fallback is also unavailable, surface the
/// integer id as a string so the row is still labeled instead of dropped.
fn model_for_enum(
    enum_id: u64,
    model_map: &HashMap<u64, String>,
    primary_enum: Option<u64>,
) -> String {
    if let Some(name) = model_map.get(&enum_id) {
        return name.clone();
    }
    if let Some(primary) = primary_enum
        && primary != enum_id
        && let Some(name) = model_map.get(&primary)
    {
        return name.clone();
    }
    enum_id.to_string()
}

/// Build the timestamp for a single step. The `f5` sub-message carries
/// the request time as a Unix-second count plus a nanosecond fraction.
/// Rows with a non-positive Unix second are skipped — the timestamp is
/// required for the downstream report shape and a zero/negative value
/// would not correspond to a real request.
fn timestamp_from_step(usage_data: &StepUsage) -> Option<TimestampMs> {
    if usage_data.timestamp_seconds <= 0 {
        return None;
    }
    let seconds_in_millis = usage_data
        .timestamp_seconds
        .checked_mul(crate::MILLIS_PER_SECOND)?;
    let nanos_in_millis = usage_data.timestamp_nanos / 1_000_000;
    let millis = seconds_in_millis.checked_add(nanos_in_millis)?;
    Some(TimestampMs::from_millis(millis))
}

/// Build the synthetic message id used for dedupe. The protobuf
/// `requestId` is preferred (it matches the upstream request id), but
/// when the request id is missing we synthesize a stable id from the
/// step's `idx` so duplicate rows still get deduplicated downstream.
fn request_id_for_entry(request_id: Option<&str>, idx: i64) -> String {
    match request_id {
        Some(id) if !id.is_empty() => id.to_string(),
        _ => format!("agy-step-{idx}"),
    }
}

#[cfg(test)]
mod tests {
    use super::super::support::{
        build_gen_metadata_row, build_step_payload, create_schema, insert_gen_metadata,
        insert_step, open_db,
    };
    use super::*;
    use ccusage_test_support::fs_fixture;

    /// Test 1: Basic token extraction. Build a single step row with all
    /// the expected token fields populated and verify the parser turns it
    /// into a `LoadedEntry` with the right values.
    #[test]
    fn extracts_basic_tokens_from_step_payload() {
        let fixture = fs_fixture!({});
        let db_path = fixture.path("conv-basic.db");
        let db = open_db(&db_path);
        create_schema(&db);
        insert_gen_metadata(&db, 0, &build_gen_metadata_row(1035, "claude-sonnet-4-6"));
        let payload =
            build_step_payload(1_767_312_000, 0, 1035, 797, 169, 22_472, Some("req-basic"));
        insert_step(&db, 1, STEP_TYPE_MODEL_RESPONSE, &payload);

        let pricing = PricingMap::load_embedded();
        let tz = jiff::tz::TimeZone::UTC;
        let entries = parse_conversation_db(&db_path, Some(&tz), CostMode::Calculate, &pricing);

        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.session_id.as_ref(), "conv-basic");
        assert_eq!(entry.project.as_ref(), "conv-basic");
        assert_eq!(entry.date, "2026-01-02");
        assert_eq!(entry.timestamp.as_millis(), 1_767_312_000_000);
        assert_eq!(entry.model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(entry.data.message.usage.input_tokens, 22_472);
        assert_eq!(entry.data.message.usage.output_tokens, 797);
        assert_eq!(entry.extra_total_tokens, 169);
        assert_eq!(entry.data.message.id.as_deref(), Some("req-basic"));
        assert_eq!(entry.data.request_id.as_deref(), Some("req-basic"));
        assert!(entry.cost > 0.0);
    }

    /// Test 2: Model mapping. Build a `gen_metadata` table with one row
    /// and verify the model name resolves from the enum id stored in
    /// `step_payload.f5.f9.f1`.
    #[test]
    fn resolves_model_name_from_gen_metadata() {
        let fixture = fs_fixture!({});
        let db_path = fixture.path("conv-model.db");
        let db = open_db(&db_path);
        create_schema(&db);
        insert_gen_metadata(&db, 0, &build_gen_metadata_row(1132, "gemini-3-flash-a"));
        let payload = build_step_payload(1_767_312_000, 0, 1132, 100, 0, 500, Some("req-model"));
        insert_step(&db, 1, STEP_TYPE_MODEL_RESPONSE, &payload);

        let pricing = PricingMap::load_embedded();
        let entries = parse_conversation_db(&db_path, None, CostMode::Display, &pricing);

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].model.as_deref(), Some("gemini-3-flash-a"));
    }

    /// Test 3: Cumulative input diff. Two steps whose cumulative input
    /// jumps from 10_000 to 10_500 must produce per-turn input values of
    /// 10_000 and 500 respectively.
    #[test]
    fn diffs_cumulative_input_across_conversation_steps() {
        let fixture = fs_fixture!({});
        let db_path = fixture.path("conv-diff.db");
        let db = open_db(&db_path);
        create_schema(&db);
        insert_gen_metadata(&db, 0, &build_gen_metadata_row(1035, "claude-sonnet-4-6"));
        let first = build_step_payload(1_767_312_000, 0, 1035, 50, 0, 10_000, Some("req-1"));
        let second = build_step_payload(1_767_312_001, 0, 1035, 60, 0, 10_500, Some("req-2"));
        insert_step(&db, 1, STEP_TYPE_MODEL_RESPONSE, &first);
        insert_step(&db, 2, STEP_TYPE_CHECKPOINT, &second);

        let pricing = PricingMap::load_embedded();
        let entries = parse_conversation_db(&db_path, None, CostMode::Display, &pricing);

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].data.message.usage.input_tokens, 10_000);
        assert_eq!(entries[1].data.message.usage.input_tokens, 500);
    }

    /// Test 3b: Checkpoint attribution. Checkpoint steps (`step_type` 23)
    /// carry an internal model enum (here 1050) that `gen_metadata` never
    /// names, so it is absent from the enum → name map. Such steps must be
    /// attributed to the conversation's primary model — the model used by the
    /// model-response steps — so their usage lands on a real, priced model id
    /// instead of a bare number.
    #[test]
    fn attributes_unmapped_checkpoint_to_primary_model() {
        let fixture = fs_fixture!({});
        let db_path = fixture.path("conv-ckpt.db");
        let db = open_db(&db_path);
        create_schema(&db);
        insert_gen_metadata(&db, 0, &build_gen_metadata_row(1035, "claude-sonnet-4-6"));
        // Two model-response steps with the mapped enum 1035 (the primary).
        let first = build_step_payload(1_767_312_000, 0, 1035, 100, 0, 1_000, Some("req-r1"));
        let second = build_step_payload(1_767_312_001, 0, 1035, 80, 0, 1_200, Some("req-r2"));
        insert_step(&db, 1, STEP_TYPE_MODEL_RESPONSE, &first);
        insert_step(&db, 2, STEP_TYPE_MODEL_RESPONSE, &second);
        // A checkpoint step whose enum (1050) is NOT in the gen_metadata map.
        let ckpt = build_step_payload(1_767_312_002, 0, 1050, 500, 0, 0, Some("req-ckpt"));
        insert_step(&db, 3, STEP_TYPE_CHECKPOINT, &ckpt);

        let pricing = PricingMap::load_embedded();
        let entries = parse_conversation_db(&db_path, None, CostMode::Display, &pricing);

        assert_eq!(entries.len(), 3);
        // Both model responses resolve directly; the checkpoint is attributed
        // to the primary model (1035 -> claude-sonnet-4-6), not "1050".
        assert_eq!(entries[0].model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(entries[1].model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(entries[2].model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(entries[2].data.message.usage.output_tokens, 500);
    }

    /// Test 4: Empty database. The tables exist but no rows are
    /// present; the parser must return an empty result rather than
    /// panicking on missing data.
    #[test]
    fn returns_empty_for_empty_database() {
        let fixture = fs_fixture!({});
        let db_path = fixture.path("conv-empty.db");
        let db = open_db(&db_path);
        create_schema(&db);

        let pricing = PricingMap::load_embedded();
        let entries = parse_conversation_db(&db_path, None, CostMode::Display, &pricing);
        assert!(entries.is_empty());
    }

    /// Test 5: Missing `gen_metadata`. Steps still exist but the model
    /// mapping table is empty. The parser must fall back to the enum id
    /// as a string so the row is still surfaced with a model label.
    #[test]
    fn falls_back_to_enum_id_when_gen_metadata_is_missing() {
        let fixture = fs_fixture!({});
        let db_path = fixture.path("conv-no-meta.db");
        let db = open_db(&db_path);
        create_schema(&db);
        let payload = build_step_payload(1_767_312_000, 0, 4242, 100, 0, 500, Some("req-no-meta"));
        insert_step(&db, 1, STEP_TYPE_MODEL_RESPONSE, &payload);

        let pricing = PricingMap::load_embedded();
        let entries = parse_conversation_db(&db_path, None, CostMode::Display, &pricing);

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].model.as_deref(), Some("4242"));
    }

    /// Test 6: Timestamp extraction. A step whose `f5` sub-message
    /// carries `f1` (seconds) and `f2` (nanoseconds) must be turned
    /// into a millisecond timestamp that matches both fields, and the
    /// RFC3339 string must be consistent with the timestamp.
    #[test]
    fn extracts_timestamp_from_step_payload() {
        let fixture = fs_fixture!({});
        let db_path = fixture.path("conv-time.db");
        let db = open_db(&db_path);
        create_schema(&db);
        insert_gen_metadata(&db, 0, &build_gen_metadata_row(1035, "claude-sonnet-4-6"));
        // 1_767_312_000 seconds + 123_456_789 nanoseconds = a timestamp
        // 123 ms past the second boundary.
        let payload =
            build_step_payload(1_767_312_000, 123_456_789, 1035, 1, 0, 1, Some("req-time"));
        insert_step(&db, 1, STEP_TYPE_MODEL_RESPONSE, &payload);

        let pricing = PricingMap::load_embedded();
        let tz = jiff::tz::TimeZone::UTC;
        let entries = parse_conversation_db(&db_path, Some(&tz), CostMode::Display, &pricing);

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].timestamp.as_millis(), 1_767_312_000_123);
        assert_eq!(entries[0].date, "2026-01-02");
    }

    /// Test 7: Skipped step types. Only `step_type IN (15, 23)` should
    /// produce a `LoadedEntry`; other step types are filtered out even
    /// when their `step_payload` would decode successfully.
    #[test]
    fn skips_unsupported_step_types() {
        let fixture = fs_fixture!({});
        let db_path = fixture.path("conv-skip.db");
        let db = open_db(&db_path);
        create_schema(&db);
        insert_gen_metadata(&db, 0, &build_gen_metadata_row(1035, "claude-sonnet-4-6"));
        let payload = build_step_payload(1_767_312_000, 0, 1035, 100, 0, 500, Some("req-skip"));
        insert_step(&db, 1, 8, &payload);
        insert_step(&db, 2, 99, &payload);
        insert_step(&db, 3, STEP_TYPE_MODEL_RESPONSE, &payload);

        let pricing = PricingMap::load_embedded();
        let entries = parse_conversation_db(&db_path, None, CostMode::Display, &pricing);

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].data.message.id.as_deref(), Some("req-skip"));
    }

    /// Characterization: thinking/extra tokens are billed at the output rate,
    /// so a step with `extra_total_tokens > 0` must cost strictly more than an
    /// otherwise-identical step with no thinking tokens, and both must be
    /// priced (the `usage_for_cost` helper must fold thinking into output
    /// before costing).
    #[test]
    fn extra_total_tokens_increase_cost() {
        let pricing = PricingMap::load_embedded();
        let tz = jiff::tz::TimeZone::UTC;

        let with_thinking = {
            let fixture = fs_fixture!({});
            let db_path = fixture.path("conv-cost-thinking.db");
            let db = open_db(&db_path);
            create_schema(&db);
            insert_gen_metadata(&db, 0, &build_gen_metadata_row(1035, "claude-sonnet-4-6"));
            let payload =
                build_step_payload(1_767_312_000, 0, 1035, 100, 169, 500, Some("req-think"));
            insert_step(&db, 1, STEP_TYPE_MODEL_RESPONSE, &payload);
            parse_conversation_db(&db_path, Some(&tz), CostMode::Calculate, &pricing)
        };
        let without_thinking = {
            let fixture = fs_fixture!({});
            let db_path = fixture.path("conv-cost-no-thinking.db");
            let db = open_db(&db_path);
            create_schema(&db);
            insert_gen_metadata(&db, 0, &build_gen_metadata_row(1035, "claude-sonnet-4-6"));
            let payload =
                build_step_payload(1_767_312_000, 0, 1035, 100, 0, 500, Some("req-no-think"));
            insert_step(&db, 1, STEP_TYPE_MODEL_RESPONSE, &payload);
            parse_conversation_db(&db_path, Some(&tz), CostMode::Calculate, &pricing)
        };

        assert_eq!(with_thinking.len(), 1);
        assert_eq!(without_thinking.len(), 1);
        let cost_with = with_thinking[0].cost;
        let cost_without = without_thinking[0].cost;
        assert!(
            cost_without > 0.0,
            "baseline output-only cost must be priced"
        );
        assert!(
            cost_with > cost_without,
            "thinking tokens folded into output must raise the cost"
        );
        // The reported output_tokens stay the real step output; only the cost
        // path sees the inflated count.
        assert_eq!(with_thinking[0].data.message.usage.output_tokens, 100);
        assert_eq!(with_thinking[0].extra_total_tokens, 169);
    }

    /// Characterization: `usage_for_cost` saturates instead of overflowing
    /// when output_tokens + extra_total_tokens would exceed u64::MAX.
    #[test]
    fn usage_for_cost_saturates_output_tokens() {
        let usage = TokenUsageRaw {
            output_tokens: u64::MAX,
            ..TokenUsageRaw::default()
        };
        let cost_usage = usage_for_cost(usage, 1_000);
        assert_eq!(cost_usage.output_tokens, u64::MAX);
    }
}
