//! Bounded record decoding shared by forward and reverse readers.

use serde::Deserialize;

use crate::record::{InvocationRecord, SCHEMA_VERSION};

#[derive(Deserialize)]
struct SchemaProbe {
    schema_version: u64,
}

pub(super) enum DecodeError {
    Corrupt(serde_json::Error),
    Unsupported(u64),
}

/// Checks the schema before decoding directly into the stable record type.
///
/// The small first pass preserves the distinction between a future schema
/// and corruption without materializing the whole document as a generic
/// `serde_json::Value` alongside the typed record.
pub(super) fn decode_record(bytes: &[u8]) -> Result<InvocationRecord, DecodeError> {
    let probe: SchemaProbe = serde_json::from_slice(bytes).map_err(DecodeError::Corrupt)?;
    if probe.schema_version != u64::from(SCHEMA_VERSION) {
        return Err(DecodeError::Unsupported(probe.schema_version));
    }
    serde_json::from_slice(bytes).map_err(DecodeError::Corrupt)
}
