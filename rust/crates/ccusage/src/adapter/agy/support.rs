//! Shared test scaffolding for the `agy` adapter test modules.
//!
//! Holds the protobuf wire-format encoders, the synthetic `step_payload` and
//! `gen_metadata` blob builders, and the in-memory SQLite fixture helpers so
//! each adapter test module states only its per-test intent instead of
//! re-deriving the same plumbing.

use std::path::Path;

// Re-export the parser's protobuf field constants so test modules reach them
// through `support::` and never redefine them.
pub(super) use super::parser::{
    FIELD_F5_TIMESTAMP_SUBMESSAGE, FIELD_F5_USAGE_SUBMESSAGE, FIELD_F9_CUMULATIVE_INPUT,
    FIELD_F9_MODEL_ENUM, FIELD_F9_OUTPUT_TOKENS, FIELD_F9_REQUEST_ID, FIELD_F9_THINKING_TOKENS,
    FIELD_GEN_METADATA_ENUM, FIELD_GEN_METADATA_INNER, FIELD_GEN_METADATA_MODEL_NAME,
    FIELD_OUTER_EVENT_SUBMESSAGE, FIELD_TS_NANOS, FIELD_TS_SECONDS, STEP_TYPE_MODEL_RESPONSE,
};

/// Encode a varint per the protobuf wire format.
pub(super) fn encode_varint(mut value: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    loop {
        if value < 0x80 {
            bytes.push(value as u8);
            return bytes;
        }
        bytes.push((value as u8 & 0x7F) | 0x80);
        value >>= 7;
    }
}

pub(super) fn encode_varint_field(field_number: u64, value: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&encode_varint(field_number << 3));
    bytes.extend_from_slice(&encode_varint(value));
    bytes
}

pub(super) fn encode_string_field(field_number: u64, value: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&encode_varint((field_number << 3) | 2));
    bytes.extend_from_slice(&encode_varint(value.len() as u64));
    bytes.extend_from_slice(value.as_bytes());
    bytes
}

pub(super) fn encode_submessage_field(field_number: u64, submessage: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&encode_varint((field_number << 3) | 2));
    bytes.extend_from_slice(&encode_varint(submessage.len() as u64));
    bytes.extend_from_slice(submessage);
    bytes
}

#[allow(clippy::too_many_arguments)]
pub(super) fn build_step_payload(
    timestamp_seconds: u64,
    timestamp_nanos: u64,
    model_enum: u64,
    output_tokens: u64,
    thinking_tokens: u64,
    cumulative_input: u64,
    request_id: Option<&str>,
) -> Vec<u8> {
    let mut f9 = Vec::new();
    f9.extend_from_slice(&encode_varint_field(FIELD_F9_MODEL_ENUM, model_enum));
    f9.extend_from_slice(&encode_varint_field(FIELD_F9_OUTPUT_TOKENS, output_tokens));
    f9.extend_from_slice(&encode_varint_field(
        FIELD_F9_THINKING_TOKENS,
        thinking_tokens,
    ));
    if cumulative_input > 0 {
        f9.extend_from_slice(&encode_varint_field(
            FIELD_F9_CUMULATIVE_INPUT,
            cumulative_input,
        ));
    }
    if let Some(request_id) = request_id {
        f9.extend_from_slice(&encode_string_field(FIELD_F9_REQUEST_ID, request_id));
    }
    // The timestamp is a nested sub-message at f5.f1: {f1=seconds, f2=nanos}.
    let mut ts = Vec::new();
    ts.extend_from_slice(&encode_varint_field(FIELD_TS_SECONDS, timestamp_seconds));
    ts.extend_from_slice(&encode_varint_field(FIELD_TS_NANOS, timestamp_nanos));
    let mut f5 = Vec::new();
    f5.extend_from_slice(&encode_submessage_field(FIELD_F5_TIMESTAMP_SUBMESSAGE, &ts));
    f5.extend_from_slice(&encode_submessage_field(FIELD_F5_USAGE_SUBMESSAGE, &f9));
    encode_submessage_field(FIELD_OUTER_EVENT_SUBMESSAGE, &f5)
}

pub(super) fn build_gen_metadata_row(enum_id: u64, model_name: &str) -> Vec<u8> {
    // The model identity lives in the f1 sub-message: f3 = enum id,
    // f19 = model name string (mirrors the real Antigravity layout).
    let mut inner = Vec::new();
    inner.extend_from_slice(&encode_varint_field(FIELD_GEN_METADATA_ENUM, enum_id));
    inner.extend_from_slice(&encode_string_field(
        FIELD_GEN_METADATA_MODEL_NAME,
        model_name,
    ));
    encode_submessage_field(FIELD_GEN_METADATA_INNER, &inner)
}

pub(super) fn open_db(path: &Path) -> sqlite::Connection {
    sqlite::open(path).unwrap()
}

pub(super) fn create_schema(db: &sqlite::Connection) {
    db.execute(
        "CREATE TABLE steps (
            idx INTEGER PRIMARY KEY,
            step_type INTEGER,
            status INTEGER,
            metadata BLOB,
            step_payload BLOB,
            step_format INTEGER
        )",
    )
    .unwrap();
    db.execute(
        "CREATE TABLE gen_metadata (
            idx INTEGER PRIMARY KEY,
            data BLOB,
            size INTEGER
        )",
    )
    .unwrap();
}

pub(super) fn insert_step(db: &sqlite::Connection, idx: i64, step_type: i64, step_payload: &[u8]) {
    let mut statement = db
        .prepare("INSERT INTO steps (idx, step_type, status, step_payload) VALUES (?1, ?2, 0, ?3)")
        .unwrap();
    statement.bind((1, idx)).unwrap();
    statement.bind((2, step_type)).unwrap();
    statement.bind((3, step_payload)).unwrap();
    statement.next().unwrap();
}

pub(super) fn insert_gen_metadata(db: &sqlite::Connection, idx: i64, data: &[u8]) {
    let mut statement = db
        .prepare("INSERT INTO gen_metadata (idx, data, size) VALUES (?1, ?2, ?3)")
        .unwrap();
    statement.bind((1, idx)).unwrap();
    statement.bind((2, data)).unwrap();
    statement.bind((3, data.len() as i64)).unwrap();
    statement.next().unwrap();
}
