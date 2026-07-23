//! Reverse, bounded reads for the newest metrics window.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::error::MetricsError;
use crate::record::{InvocationRecord, SCHEMA_VERSION};

use super::decode::{DecodeError, decode_record};
use super::{MAX_RECORD_BYTES, MetricsStore};

/// Two maximum-sized records keep normal small histories at the requested
/// 20 entries while bounding typed record allocations for adversarial local
/// files. `read_recent` is explicitly an "up to" API and may return fewer.
const MAX_RECENT_SERIALIZED_BYTES: usize = 2 * MAX_RECORD_BYTES;

/// Blank lines consume no serialized-record budget. This independent ceiling
/// prevents a file containing only delimiters from turning a recent read into
/// an unbounded reverse scan.
const MAX_RECENT_LINES_SCANNED: usize = 256;
const REVERSE_SCAN_CHUNK_BYTES: usize = 8 * 1024;

enum PreviousLine {
    Line {
        bytes: Vec<u8>,
        start: u64,
        next_cursor: u64,
        terminated: bool,
    },
    AggregateBudgetExhausted,
}

impl MetricsStore {
    pub(super) fn read_recent_records(
        &self,
        limit: usize,
    ) -> Result<Vec<InvocationRecord>, MetricsError> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        let mut file = match self.open_for_read()? {
            Some(file) => file,
            None => return Ok(Vec::new()),
        };
        file.lock_shared()
            .map_err(|source| MetricsError::LockFile {
                path: self.file_path.clone(),
                source,
            })?;

        let mut cursor = file
            .metadata()
            .map_err(|source| MetricsError::ReadFile {
                path: self.file_path.clone(),
                source,
            })?
            .len();
        let mut records = VecDeque::new();
        let mut serialized_bytes = 0usize;
        let mut lines_scanned = 0usize;

        while cursor > 0
            && records.len() < limit
            && serialized_bytes < MAX_RECENT_SERIALIZED_BYTES
            && lines_scanned < MAX_RECENT_LINES_SCANNED
        {
            let remaining_budget = MAX_RECENT_SERIALIZED_BYTES - serialized_bytes;
            let max_content = remaining_budget.min(MAX_RECORD_BYTES);
            let previous = read_previous_line(&mut file, cursor, max_content, &self.file_path)?;
            let PreviousLine::Line {
                bytes,
                start,
                next_cursor,
                terminated,
            } = previous
            else {
                break;
            };

            cursor = next_cursor;
            lines_scanned = lines_scanned.saturating_add(1);
            serialized_bytes = serialized_bytes.saturating_add(bytes.len());
            let trimmed = bytes.trim_ascii();
            if trimmed.is_empty() {
                continue;
            }

            match decode_record(trimmed) {
                Ok(record) => records.push_front(record),
                // The final unterminated fragment may be a torn append. It
                // is ignored exactly as it is by the complete-history reader.
                Err(DecodeError::Corrupt(_)) if !terminated => {}
                Err(DecodeError::Corrupt(source)) => {
                    return Err(MetricsError::CorruptRecentRecord {
                        path: self.file_path.clone(),
                        offset: start,
                        source,
                    });
                }
                Err(DecodeError::Unsupported(found)) => {
                    return Err(MetricsError::UnsupportedRecentSchema {
                        path: self.file_path.clone(),
                        offset: start,
                        found,
                        supported: SCHEMA_VERSION,
                    });
                }
            }
        }

        Ok(records.into_iter().collect())
    }
}

fn read_previous_line(
    file: &mut File,
    cursor: u64,
    max_content: usize,
    path: &Path,
) -> Result<PreviousLine, MetricsError> {
    let mut final_byte = [0_u8; 1];
    read_exact_at(file, cursor - 1, &mut final_byte, path)?;
    let terminated = final_byte[0] == b'\n';
    let line_end = if terminated { cursor - 1 } else { cursor };
    if line_end == 0 {
        return Ok(PreviousLine::Line {
            bytes: Vec::new(),
            start: 0,
            next_cursor: 0,
            terminated,
        });
    }

    // Include one delimiter byte before the maximum content length. This
    // distinguishes an exactly-at-limit record from an oversized one while
    // every candidate record remains capped at `MAX_RECORD_BYTES`.
    let search_width = max_content.saturating_add(1) as u64;
    let search_floor = line_end.saturating_sub(search_width);
    let mut scan_cursor = line_end;
    let mut chunk = [0_u8; REVERSE_SCAN_CHUNK_BYTES];
    while scan_cursor > search_floor {
        let bytes_to_read =
            (scan_cursor - search_floor).min(REVERSE_SCAN_CHUNK_BYTES as u64) as usize;
        scan_cursor -= bytes_to_read as u64;
        read_exact_at(file, scan_cursor, &mut chunk[..bytes_to_read], path)?;
        if let Some(index) = chunk[..bytes_to_read]
            .iter()
            .rposition(|byte| *byte == b'\n')
        {
            let start = scan_cursor + index as u64 + 1;
            let bytes = read_content(file, start, line_end - start, path)?;
            return Ok(PreviousLine::Line {
                bytes,
                start,
                next_cursor: start,
                terminated,
            });
        }
    }

    if search_floor == 0 && line_end <= max_content as u64 {
        let bytes = read_content(file, 0, line_end, path)?;
        return Ok(PreviousLine::Line {
            bytes,
            start: 0,
            next_cursor: 0,
            terminated,
        });
    }

    if max_content < MAX_RECORD_BYTES {
        Ok(PreviousLine::AggregateBudgetExhausted)
    } else {
        Err(MetricsError::RecordReadLimit {
            path: path.to_path_buf(),
            max_bytes: MAX_RECORD_BYTES,
        })
    }
}

fn read_content(
    file: &mut File,
    start: u64,
    len: u64,
    path: &Path,
) -> Result<Vec<u8>, MetricsError> {
    let len = usize::try_from(len).map_err(|_| MetricsError::RecordReadLimit {
        path: path.to_path_buf(),
        max_bytes: MAX_RECORD_BYTES,
    })?;
    if len > MAX_RECORD_BYTES {
        return Err(MetricsError::RecordReadLimit {
            path: path.to_path_buf(),
            max_bytes: MAX_RECORD_BYTES,
        });
    }
    let mut bytes = vec![0_u8; len];
    read_exact_at(file, start, &mut bytes, path)?;
    Ok(bytes)
}

fn read_exact_at(
    file: &mut File,
    offset: u64,
    bytes: &mut [u8],
    path: &Path,
) -> Result<(), MetricsError> {
    file.seek(SeekFrom::Start(offset))
        .and_then(|_| file.read_exact(bytes))
        .map_err(|source| MetricsError::ReadFile {
            path: path.to_path_buf(),
            source,
        })
}
