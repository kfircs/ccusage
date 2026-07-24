//! Raw protobuf field traversal decoder.
//!
//! Reads protobuf binary blobs without a `.proto` schema file, using only
//! raw field numbers and wire types. This is a low-level utility used by
//! the `agy` adapter parser to extract fields from protobuf-encoded usage
//! data.

/// Wire type: varint.
const WIRE_TYPE_VARINT: u32 = 0;

/// Wire type: 64-bit fixed.
const WIRE_TYPE_64BIT: u32 = 1;

/// Wire type: length-delimited (strings, nested messages, packed repeated).
const WIRE_TYPE_LENGTH_DELIMITED: u32 = 2;

/// Wire type: start group (deprecated).
const WIRE_TYPE_START_GROUP: u32 = 3;

/// Wire type: end group (deprecated).
const WIRE_TYPE_END_GROUP: u32 = 4;

/// Wire type: 32-bit fixed.
const WIRE_TYPE_32BIT: u32 = 5;

/// Read a base-128 varint from `data` starting at `*pos`.
///
/// Each byte contributes its low 7 bits; the high bit (0x80) signals
/// continuation. Advances `*pos` past the varint. Returns `None` if data
/// is exhausted or the varint exceeds 10 bytes (the maximum for a 64-bit
/// varint).
pub(super) fn read_varint(data: &[u8], pos: &mut usize) -> Option<u64> {
    let mut result: u64 = 0;
    let mut shift: u32 = 0;
    let start = *pos;

    loop {
        if *pos >= data.len() {
            // Out of data — restore position so callers can detect failure.
            *pos = start;
            return None;
        }
        let byte = data[*pos];
        *pos += 1;

        result |= u64::from(byte & 0x7F).checked_shl(shift)?;
        if byte & 0x80 == 0 {
            return Some(result);
        }
        shift += 7;
        if shift > 63 {
            // Varint is too long to fit in a u64.
            *pos = start;
            return None;
        }
    }
}

/// Extract the first field matching `field_number` and return its raw value
/// bytes as a slice into the original data.
pub(super) fn extract_field(data: &[u8], field_number: u64) -> Option<&[u8]> {
    let mut pos = 0usize;

    while pos < data.len() {
        let field_start = pos;

        let tag = read_varint(data, &mut pos)?;
        let wire_type = (tag & 0x7) as u32;
        let current_field_number = tag >> 3;

        match wire_type {
            WIRE_TYPE_VARINT => {
                let varint_start = pos;
                read_varint(data, &mut pos)?;
                if current_field_number == field_number {
                    return Some(&data[varint_start..pos]);
                }
            }
            WIRE_TYPE_64BIT => {
                if pos.checked_add(8).is_none_or(|end| end > data.len()) {
                    return None;
                }
                if current_field_number == field_number {
                    return Some(&data[pos..pos + 8]);
                }
                pos += 8;
            }
            WIRE_TYPE_LENGTH_DELIMITED => {
                let length = read_varint(data, &mut pos)?;
                let length = usize::try_from(length).ok()?;
                if pos.checked_add(length).is_none_or(|end| end > data.len()) {
                    return None;
                }
                if current_field_number == field_number {
                    return Some(&data[pos..pos + length]);
                }
                pos += length;
            }
            WIRE_TYPE_32BIT => {
                if pos.checked_add(4).is_none_or(|end| end > data.len()) {
                    return None;
                }
                if current_field_number == field_number {
                    return Some(&data[pos..pos + 4]);
                }
                pos += 4;
            }
            WIRE_TYPE_START_GROUP | WIRE_TYPE_END_GROUP => {
                // Deprecated group wire types — skip silently.
                pos = field_start;
                let _ = read_varint(data, &mut pos);
            }
            _ => return None,
        }
    }

    None
}

/// Extract the first field matching `field_number` as a varint.
pub(super) fn extract_varint(data: &[u8], field_number: u64) -> Option<u64> {
    let raw = extract_field(data, field_number)?;
    let mut pos = 0usize;
    read_varint(raw, &mut pos)
}

/// Extract the first field matching `field_number` as a length-delimited
/// sub-message, returning a slice into the original data.
pub(super) fn extract_length_delimited(data: &[u8], field_number: u64) -> Option<&[u8]> {
    let mut pos = 0usize;

    while pos < data.len() {
        let field_start = pos;

        let tag = read_varint(data, &mut pos)?;
        let wire_type = (tag & 0x7) as u32;
        let current_field_number = tag >> 3;

        match wire_type {
            WIRE_TYPE_VARINT => {
                read_varint(data, &mut pos)?;
                if current_field_number == field_number {
                    return None;
                }
            }
            WIRE_TYPE_64BIT => {
                if pos.checked_add(8).is_none_or(|end| end > data.len()) {
                    return None;
                }
                pos += 8;
            }
            WIRE_TYPE_LENGTH_DELIMITED => {
                let length = read_varint(data, &mut pos)?;
                let length = usize::try_from(length).ok()?;
                if pos.checked_add(length).is_none_or(|end| end > data.len()) {
                    return None;
                }
                if current_field_number == field_number {
                    return Some(&data[pos..pos + length]);
                }
                pos += length;
            }
            WIRE_TYPE_32BIT => {
                if pos.checked_add(4).is_none_or(|end| end > data.len()) {
                    return None;
                }
                pos += 4;
            }
            WIRE_TYPE_START_GROUP | WIRE_TYPE_END_GROUP => {
                pos = field_start;
                let _ = read_varint(data, &mut pos);
            }
            _ => return None,
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A single protobuf field extracted from raw binary data.
    #[derive(Debug, Clone)]
    struct RawField {
        field_number: u64,
        wire_type: u32,
        data: Vec<u8>,
    }

    /// Extract all top-level fields from raw protobuf data.
    ///
    /// Iterates through `data`, parsing each tag (field_number << 3 | wire_type)
    /// and the corresponding value. Fields with deprecated group wire types
    /// (3 and 4) are silently skipped. Malformed data causes the function to
    /// stop iterating and return whatever fields were successfully parsed so far.
    fn extract_all_fields(data: &[u8]) -> Vec<RawField> {
        let mut fields = Vec::new();
        let mut pos = 0usize;

        while pos < data.len() {
            let field_start = pos;

            let tag = match read_varint(data, &mut pos) {
                Some(tag) => tag,
                None => break,
            };

            let wire_type = (tag & 0x7) as u32;
            let field_number = tag >> 3;

            // Read the value based on the wire type.
            let field_data = match wire_type {
                WIRE_TYPE_VARINT => {
                    let varint_start = pos;
                    match read_varint(data, &mut pos) {
                        Some(_) => data[varint_start..pos].to_vec(),
                        None => break,
                    }
                }
                WIRE_TYPE_64BIT => {
                    if pos.checked_add(8).is_none_or(|end| end > data.len()) {
                        break;
                    }
                    let chunk = data[pos..pos + 8].to_vec();
                    pos += 8;
                    chunk
                }
                WIRE_TYPE_LENGTH_DELIMITED => {
                    let length = match read_varint(data, &mut pos) {
                        Some(length) => length,
                        None => break,
                    };
                    let length = match usize::try_from(length) {
                        Ok(length) => length,
                        Err(_) => break,
                    };
                    if pos.checked_add(length).is_none_or(|end| end > data.len()) {
                        break;
                    }
                    let chunk = data[pos..pos + length].to_vec();
                    pos += length;
                    chunk
                }
                WIRE_TYPE_32BIT => {
                    if pos.checked_add(4).is_none_or(|end| end > data.len()) {
                        break;
                    }
                    let chunk = data[pos..pos + 4].to_vec();
                    pos += 4;
                    chunk
                }
                WIRE_TYPE_START_GROUP | WIRE_TYPE_END_GROUP => {
                    // Deprecated group wire types — skip silently.
                    pos = field_start;
                    // Re-read the tag to advance past it, then continue.
                    let _ = read_varint(data, &mut pos);
                    continue;
                }
                _ => {
                    // Unknown wire type — cannot determine field length, stop.
                    break;
                }
            };

            fields.push(RawField {
                field_number,
                wire_type,
                data: field_data,
            });
        }

        fields
    }

    // -- read_varint tests --------------------------------------------------

    #[test]
    fn reads_single_byte_varint() {
        let data = [0x01u8];
        let mut pos = 0;
        let value = read_varint(&data, &mut pos);
        assert_eq!(value, Some(1));
        assert_eq!(pos, 1);
    }

    #[test]
    fn reads_zero_varint() {
        let data = [0x00u8];
        let mut pos = 0;
        let value = read_varint(&data, &mut pos);
        assert_eq!(value, Some(0));
        assert_eq!(pos, 1);
    }

    #[test]
    fn reads_two_byte_varint() {
        // 0xAC 0x02 → 300
        let data = [0xACu8, 0x02];
        let mut pos = 0;
        let value = read_varint(&data, &mut pos);
        assert_eq!(value, Some(300));
        assert_eq!(pos, 2);
    }

    #[test]
    fn reads_large_varint() {
        // 300 is a common value: 0xAC 0x02
        let data = [0xACu8, 0x02];
        let mut pos = 0;
        assert_eq!(read_varint(&data, &mut pos), Some(300));
    }

    #[test]
    fn reads_max_varint_10_bytes() {
        // Maximum u64 value: 0xFFFFFFFFFFFFFFFF encoded as 10-byte varint
        let data = [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01];
        let mut pos = 0;
        let value = read_varint(&data, &mut pos);
        assert_eq!(value, Some(u64::MAX));
        assert_eq!(pos, 10);
    }

    #[test]
    fn returns_none_on_exhausted_data() {
        let data: [u8; 0] = [];
        let mut pos = 0;
        assert_eq!(read_varint(&data, &mut pos), None);
        assert_eq!(pos, 0);
    }

    #[test]
    fn returns_none_on_truncated_varint() {
        // Continuation bit set but no following byte
        let data = [0x80u8];
        let mut pos = 0;
        assert_eq!(read_varint(&data, &mut pos), None);
        assert_eq!(pos, 0); // position should be restored
    }

    #[test]
    fn returns_none_on_overlong_varint() {
        // 11 bytes all with continuation bit set — exceeds 10-byte limit
        let data = [
            0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x01,
        ];
        let mut pos = 0;
        assert_eq!(read_varint(&data, &mut pos), None);
        assert_eq!(pos, 0);
    }

    // -- extract_all_fields tests -------------------------------------------

    #[test]
    fn extracts_simple_varint_field() {
        // Field 1, wire type 0 (varint), value 1
        // tag = (1 << 3) | 0 = 0x08, value = 0x01
        let data = [0x08, 0x01];
        let fields = extract_all_fields(&data);
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].field_number, 1);
        assert_eq!(fields[0].wire_type, 0);
        assert_eq!(fields[0].data, [0x01]);
    }

    #[test]
    fn extracts_multi_byte_varint_field() {
        // Field 1, wire type 0, value 300
        // tag = 0x08, value = 0xAC 0x02
        let data = [0x08, 0xAC, 0x02];
        let fields = extract_all_fields(&data);
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].field_number, 1);
        assert_eq!(fields[0].wire_type, 0);
        assert_eq!(fields[0].data, [0xAC, 0x02]);
    }

    #[test]
    fn extracts_multiple_fields() {
        // Field 1 (varint) = 150 → tag 0x08, value 0x96 0x01
        // Field 2 (length-delimited) = "abc" → tag 0x12, len 0x03, "abc"
        let data = [0x08, 0x96, 0x01, 0x12, 0x03, b'a', b'b', b'c'];
        let fields = extract_all_fields(&data);
        assert_eq!(fields.len(), 2);

        assert_eq!(fields[0].field_number, 1);
        assert_eq!(fields[0].wire_type, 0);
        assert_eq!(fields[0].data, [0x96, 0x01]);

        assert_eq!(fields[1].field_number, 2);
        assert_eq!(fields[1].wire_type, 2);
        assert_eq!(fields[1].data, b"abc");
    }

    #[test]
    fn extracts_64bit_field() {
        // Field 3, wire type 1 (64-bit), 8 bytes of data
        // tag = (3 << 3) | 1 = 0x19
        let data = [0x19, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
        let fields = extract_all_fields(&data);
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].field_number, 3);
        assert_eq!(fields[0].wire_type, 1);
        assert_eq!(
            fields[0].data,
            [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]
        );
    }

    #[test]
    fn extracts_32bit_field() {
        // Field 5, wire type 5 (32-bit), 4 bytes of data
        // tag = (5 << 3) | 5 = 0x2D
        let data = [0x2D, 0x01, 0x02, 0x03, 0x04];
        let fields = extract_all_fields(&data);
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].field_number, 5);
        assert_eq!(fields[0].wire_type, 5);
        assert_eq!(fields[0].data, [0x01, 0x02, 0x03, 0x04]);
    }

    #[test]
    fn extracts_nested_submessage() {
        // Field 2 (length-delimited) containing a nested message:
        //   nested: field 1 (varint) = 42 → tag 0x08, value 0x2A
        // Outer tag = 0x12, length = 0x02
        let nested = [0x08, 0x2A];
        let data = [0x12, 0x02, nested[0], nested[1]];
        let fields = extract_all_fields(&data);
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].field_number, 2);
        assert_eq!(fields[0].wire_type, 2);
        assert_eq!(fields[0].data, nested);
    }

    #[test]
    fn handles_empty_data() {
        let fields = extract_all_fields(&[]);
        assert!(fields.is_empty());
    }

    #[test]
    fn handles_truncated_tag() {
        // Only a continuation byte with no follow-up — tag is incomplete
        let data = [0x80];
        let fields = extract_all_fields(&data);
        assert!(fields.is_empty());
    }

    #[test]
    fn handles_truncated_varint_value() {
        // Field 1, wire type 0, but value varint is truncated
        let data = [0x08, 0x80];
        let fields = extract_all_fields(&data);
        assert!(fields.is_empty());
    }

    #[test]
    fn handles_truncated_64bit_field() {
        // Field 1, wire type 1, but only 4 bytes follow
        let data = [0x09, 0x01, 0x02, 0x03, 0x04];
        let fields = extract_all_fields(&data);
        assert!(fields.is_empty());
    }

    #[test]
    fn handles_truncated_32bit_field() {
        // Field 1, wire type 5, but only 2 bytes follow
        let data = [0x0D, 0x01, 0x02];
        let fields = extract_all_fields(&data);
        assert!(fields.is_empty());
    }

    #[test]
    fn handles_truncated_length_delimited() {
        // Field 2, wire type 2, length says 10 bytes but only 3 follow
        let data = [0x12, 0x0A, b'a', b'b', b'c'];
        let fields = extract_all_fields(&data);
        assert!(fields.is_empty());
    }

    #[test]
    fn does_not_panic_on_unknown_wire_type() {
        // Field 1, wire type 6 (invalid) — tag = (1 << 3) | 6 = 0x0E
        let data = [0x0E];
        let fields = extract_all_fields(&data);
        assert!(fields.is_empty());
    }

    #[test]
    fn skips_deprecated_group_wire_types() {
        // Field 1, wire type 3 (start group) then field 2, wire type 0
        // tag for start group = (1 << 3) | 3 = 0x0B
        // tag for varint = (2 << 3) | 0 = 0x10, value 0x01
        let data = [0x0B, 0x10, 0x01];
        let fields = extract_all_fields(&data);
        // Group start is skipped, varint field 2 should be parsed
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].field_number, 2);
        assert_eq!(fields[0].wire_type, 0);
        assert_eq!(fields[0].data, [0x01]);
    }

    // -- extract_field tests ------------------------------------------------

    #[test]
    fn extract_field_returns_varint_bytes() {
        // Field 1, varint value 1
        let data = [0x08, 0x01];
        let result = extract_field(&data, 1);
        assert_eq!(result, Some(&[0x01u8][..]));
    }

    #[test]
    fn extract_field_returns_none_for_missing_field() {
        let data = [0x08, 0x01];
        let result = extract_field(&data, 99);
        assert_eq!(result, None);
    }

    #[test]
    fn extract_field_returns_none_for_empty_data() {
        let result = extract_field(&[], 1);
        assert_eq!(result, None);
    }

    #[test]
    fn extract_field_returns_length_delimited_bytes() {
        // Field 2, length-delimited, value "hello"
        let data = [0x12, 0x05, b'h', b'e', b'l', b'l', b'o'];
        let result = extract_field(&data, 2);
        assert_eq!(result, Some(b"hello" as &[u8]));
    }

    #[test]
    fn extract_field_returns_64bit_bytes() {
        // Field 3, 64-bit
        let data = [0x19, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
        let result = extract_field(&data, 3);
        assert_eq!(
            result,
            Some(&[0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08][..])
        );
    }

    #[test]
    fn extract_field_returns_32bit_bytes() {
        // Field 5, 32-bit
        let data = [0x2D, 0x01, 0x02, 0x03, 0x04];
        let result = extract_field(&data, 5);
        assert_eq!(result, Some(&[0x01, 0x02, 0x03, 0x04][..]));
    }

    #[test]
    fn extract_field_returns_first_occurrence() {
        // Two fields with field number 1
        let data = [0x08, 0x01, 0x08, 0x02];
        let result = extract_field(&data, 1);
        assert_eq!(result, Some(&[0x01u8][..]));
    }

    // -- extract_varint tests -----------------------------------------------

    #[test]
    fn extract_varint_returns_decoded_value() {
        // Field 1, varint value 1
        let data = [0x08, 0x01];
        assert_eq!(extract_varint(&data, 1), Some(1));
    }

    #[test]
    fn extract_varint_returns_multi_byte_value() {
        // Field 1, varint value 300
        let data = [0x08, 0xAC, 0x02];
        assert_eq!(extract_varint(&data, 1), Some(300));
    }

    #[test]
    fn extract_varint_returns_none_for_missing_field() {
        let data = [0x08, 0x01];
        assert_eq!(extract_varint(&data, 99), None);
    }

    #[test]
    fn extract_varint_returns_none_for_empty_data() {
        assert_eq!(extract_varint(&[], 1), None);
    }

    #[test]
    fn extract_varint_handles_large_field_numbers() {
        // Field 1000, wire type 0, value 42
        // tag = (1000 << 3) | 0 = 8000 = 0x1F40
        // encoded as varint: 0xC0 0x3E (but let's compute properly)
        // 8000 in varint: 8000 = 0x1F40
        // byte 1: 0x40 | 0x80 = 0xC0 (low 7 bits = 0x40 = 64, continuation)
        // 8000 >> 7 = 62 = 0x3E
        // byte 2: 0x3E (no continuation)
        let data = [0xC0, 0x3E, 0x2A];
        assert_eq!(extract_varint(&data, 1000), Some(42));
    }

    // -- extract_length_delimited tests -------------------------------------

    #[test]
    fn extract_length_delimited_returns_inner_bytes() {
        // Field 2, length-delimited, value "hello"
        let data = [0x12, 0x05, b'h', b'e', b'l', b'l', b'o'];
        let result = extract_length_delimited(&data, 2);
        assert_eq!(result, Some(b"hello" as &[u8]));
    }

    #[test]
    fn extract_length_delimited_returns_none_for_missing_field() {
        let data = [0x08, 0x01];
        assert_eq!(extract_length_delimited(&data, 2), None);
    }

    #[test]
    fn extract_length_delimited_returns_none_for_empty_data() {
        assert_eq!(extract_length_delimited(&[], 1), None);
    }

    #[test]
    fn extract_length_delimited_returns_none_for_non_length_delimited_field() {
        // Field 1 is a varint, not length-delimited
        let data = [0x08, 0x01];
        assert_eq!(extract_length_delimited(&data, 1), None);
    }

    #[test]
    fn extract_length_delimited_returns_empty_slice_for_zero_length() {
        // Field 2, length-delimited, length 0
        let data = [0x12, 0x00];
        let result = extract_length_delimited(&data, 2);
        assert_eq!(result, Some(&[][..]));
    }

    // -- complex nested message tests ---------------------------------------

    #[test]
    fn extracts_from_complex_nested_message() {
        // Outer message:
        //   field 1 (varint) = 42
        //   field 2 (length-delimited) = nested message:
        //     field 1 (varint) = 100
        //     field 2 (length-delimited) = "test"
        //   field 3 (varint) = 7
        let nested = [
            0x08, 0x64, // field 1 = 100
            0x12, 0x04, b't', b'e', b's', b't', // field 2 = "test"
        ];
        let mut data = vec![
            0x08,
            0x2A, // field 1 = 42
            0x12,
            nested.len() as u8, // field 2 = nested message
        ];
        data.extend_from_slice(&nested);
        data.push(0x18); // field 3, wire type 0 → tag = (3 << 3) | 0 = 0x18
        data.push(0x07); // value 7

        // Extract the nested sub-message.
        let nested_slice = extract_length_delimited(&data, 2).unwrap();
        assert_eq!(nested_slice, &nested[..]);

        // Parse inside the nested message.
        assert_eq!(extract_varint(nested_slice, 1), Some(100));
        assert_eq!(
            extract_length_delimited(nested_slice, 2),
            Some(b"test" as &[u8])
        );

        // Verify outer fields too.
        assert_eq!(extract_varint(&data, 1), Some(42));
        assert_eq!(extract_varint(&data, 3), Some(7));

        // Verify all fields at the top level.
        let fields = extract_all_fields(&data);
        assert_eq!(fields.len(), 3);
        assert_eq!(fields[0].field_number, 1);
        assert_eq!(fields[1].field_number, 2);
        assert_eq!(fields[2].field_number, 3);
    }

    #[test]
    fn handles_malformed_nested_message() {
        // Outer field 2 says length 10 but only 2 bytes follow
        let data = [0x12, 0x0A, 0x01, 0x02];
        let result = extract_length_delimited(&data, 2);
        assert_eq!(result, None);
    }

    #[test]
    fn extracts_varint_with_high_field_number_from_nested() {
        // Nested message with field 15, varint value 999
        // tag = (15 << 3) | 0 = 120 = 0x78
        // 999 in varint: 999 = 0x3E7
        //   byte 1: 0x67 | 0x80 = 0xE7 (low 7 bits = 0x67 = 103, continuation)
        //   999 >> 7 = 7
        //   byte 2: 0x07 (no continuation)
        let nested = [0x78, 0xE7, 0x07];
        let data = [0x12, nested.len() as u8, nested[0], nested[1], nested[2]];

        let sub = extract_length_delimited(&data, 2).unwrap();
        assert_eq!(extract_varint(sub, 15), Some(999));
    }

    #[test]
    fn extract_all_fields_preserves_field_data_bytes() {
        // Field 2, length-delimited, value [0x08, 0x01] (looks like nested)
        let data = [0x12, 0x02, 0x08, 0x01];
        let fields = extract_all_fields(&data);
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].data, [0x08, 0x01]);
    }

    #[test]
    fn stops_gracefully_on_truncated_data_mid_stream() {
        // Field 1 (varint) = 1, then truncated field 2 (length-delimited)
        let data = [0x08, 0x01, 0x12, 0x05, b'a'];
        let fields = extract_all_fields(&data);
        // Only field 1 should be extracted, field 2 is truncated
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].field_number, 1);
    }

    #[test]
    fn extract_field_skips_unknown_fields_to_find_target() {
        // Field 1 (varint) = 1
        // Field 2 (length-delimited) = "ab"
        // Field 3 (varint) = 42
        let data = [0x08, 0x01, 0x12, 0x02, b'a', b'b', 0x18, 0x2A];
        assert_eq!(extract_varint(&data, 3), Some(42));
        assert_eq!(extract_length_delimited(&data, 2), Some(b"ab" as &[u8]));
        assert_eq!(extract_varint(&data, 1), Some(1));
    }
}
