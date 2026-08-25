use crate::error::{CherryDbError, Result};
use crate::types::DatabaseEntry;
use rusty_leveldb::{LdbIterator, Options, DB};
use serde_json::Value;

/// Decode UTF-16 LE bytes to a JSON Value
/// Cherry Studio stores data with a header byte (0x00) followed by UTF-16 LE encoded JSON
pub(crate) fn decode_utf16_le_bytes(bytes: &[u8]) -> Result<Value> {
    if bytes.is_empty() {
        return Err(CherryDbError::EncodingError("Empty bytes".to_string()));
    }

    // Skip the header byte (0x00) and convert remaining bytes to u16.
    let (pairs, leftover) = bytes[1..].as_chunks::<2>();
    if !leftover.is_empty() {
        return Err(CherryDbError::EncodingError(
            "Invalid UTF-16 data length".to_string(),
        ));
    }

    let utf16_chars: Vec<u16> = pairs.iter().copied().map(u16::from_le_bytes).collect();

    // Convert UTF-16 to String
    let json_string = String::from_utf16(&utf16_chars)
        .map_err(|_| CherryDbError::EncodingError("Invalid UTF-16 data".to_string()))?;

    // Parse as JSON
    serde_json::from_str(&json_string).map_err(CherryDbError::from)
}

/// Encode JSON Value to UTF-16 LE bytes with header
pub(crate) fn encode_json_to_bytes(json: &Value) -> Vec<u8> {
    let json_string = json.to_string();
    let utf16_chars: Vec<u16> = json_string.encode_utf16().collect();

    let mut result = vec![0x00]; // Header byte
    for char in utf16_chars {
        result.extend_from_slice(&char.to_le_bytes());
    }

    result
}

/// Open database and read all entries with decoded JSON data
pub(crate) fn open_database_and_read_entries(db_path: &str) -> Result<Vec<DatabaseEntry>> {
    if !std::path::Path::new(db_path).exists() {
        return Err(CherryDbError::InvalidPath(db_path.to_string()));
    }

    let options = Options {
        create_if_missing: false,
        ..Default::default()
    };

    let mut db = DB::open(db_path, options)
        .map_err(|e| CherryDbError::DatabaseError(format!("Failed to open database: {:?}", e)))?;

    let mut entries = Vec::new();
    let mut iter = db
        .new_iter()
        .map_err(|e| CherryDbError::DatabaseError(format!("Failed to create iterator: {:?}", e)))?;

    while let Some((key, value)) = iter.next() {
        // Try to decode as JSON
        let json_data = decode_utf16_le_bytes(&value).ok();

        entries.push(DatabaseEntry {
            key,
            value,
            json_data,
        });
    }

    Ok(entries)
}
