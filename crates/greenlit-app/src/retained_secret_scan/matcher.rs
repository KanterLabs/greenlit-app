use anyhow::Result;

use super::{ScanBudget, resource_error};

const MAX_SENSITIVE_VALUES: usize = 256;
const MAX_SENSITIVE_VALUE_BYTES: usize = 64 * 1024;
const MAX_PATTERN_COUNT: usize = 1_024;
const MAX_PATTERN_BYTES: usize = 4 * 1024 * 1024;

pub(super) struct Matcher {
    patterns: Vec<Pattern>,
}

impl Matcher {
    pub(super) fn new<I, V>(values: I) -> Result<Self>
    where
        I: IntoIterator<Item = V>,
        V: AsRef<[u8]>,
    {
        let mut raw_patterns = Vec::new();
        let mut input_bytes = 0_usize;
        for (index, value) in values.into_iter().enumerate() {
            if index >= MAX_SENSITIVE_VALUES {
                return Err(resource_error("sensitive-value-count"));
            }
            let value = value.as_ref();
            input_bytes = input_bytes
                .checked_add(value.len())
                .ok_or_else(|| resource_error("sensitive-value-bytes"))?;
            if value.len() > MAX_SENSITIVE_VALUE_BYTES || input_bytes > MAX_PATTERN_BYTES {
                return Err(resource_error("sensitive-value-bytes"));
            }
            add_candidate(value, &mut raw_patterns)?;
            if value.contains(&b'\n') {
                for line in value.split(|byte| *byte == b'\n') {
                    add_candidate(line.strip_suffix(b"\r").unwrap_or(line), &mut raw_patterns)?;
                }
            }
        }
        Ok(Matcher {
            patterns: raw_patterns.into_iter().map(Pattern::new).collect(),
        })
    }

    pub(super) fn len(&self) -> usize {
        self.patterns.len()
    }

    pub(super) fn matches(
        &self,
        bytes: &[u8],
        states: &mut [usize],
        budget: &mut ScanBudget,
    ) -> Result<bool> {
        budget.charge_match_work(bytes.len(), self.patterns.len())?;
        for (pattern, state) in self.patterns.iter().zip(states) {
            for byte in bytes {
                while *state > 0 && pattern.bytes[*state] != *byte {
                    *state = pattern.failure[*state - 1];
                }
                if pattern.bytes[*state] == *byte {
                    *state += 1;
                }
                if *state == pattern.bytes.len() {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
}

struct Pattern {
    bytes: Vec<u8>,
    failure: Vec<usize>,
}

impl Pattern {
    fn new(bytes: Vec<u8>) -> Self {
        let mut failure = vec![0; bytes.len()];
        let mut prefix = 0;
        for index in 1..bytes.len() {
            while prefix > 0 && bytes[index] != bytes[prefix] {
                prefix = failure[prefix - 1];
            }
            if bytes[index] == bytes[prefix] {
                prefix += 1;
            }
            failure[index] = prefix;
        }
        Pattern { bytes, failure }
    }
}

fn add_candidate(candidate: &[u8], patterns: &mut Vec<Vec<u8>>) -> Result<()> {
    if candidate.is_empty() || candidate.iter().all(u8::is_ascii_whitespace) {
        return Ok(());
    }
    add_pattern(candidate.to_vec(), patterns)?;
    if candidate.len() >= 4 {
        add_pattern(base64(candidate, b'+', b'/', true), patterns)?;
        add_pattern(base64(candidate, b'-', b'_', false), patterns)?;
        let percent = percent_encode(candidate);
        if percent != candidate {
            add_pattern(percent, patterns)?;
        }
    }
    Ok(())
}

fn add_pattern(pattern: Vec<u8>, patterns: &mut Vec<Vec<u8>>) -> Result<()> {
    if patterns.contains(&pattern) {
        return Ok(());
    }
    let total_bytes = patterns
        .iter()
        .try_fold(pattern.len(), |total, item| total.checked_add(item.len()));
    if patterns.len() >= MAX_PATTERN_COUNT
        || total_bytes.is_none_or(|bytes| bytes > MAX_PATTERN_BYTES)
    {
        return Err(resource_error("sensitive-patterns"));
    }
    patterns.push(pattern);
    Ok(())
}

fn base64(bytes: &[u8], char62: u8, char63: u8, padded: bool) -> Vec<u8> {
    let mut alphabet = *b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    alphabet[62] = char62;
    alphabet[63] = char63;
    let mut encoded = Vec::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or_default();
        let third = chunk.get(2).copied().unwrap_or_default();
        encoded.push(alphabet[usize::from(first >> 2)]);
        encoded.push(alphabet[usize::from(((first & 0x03) << 4) | (second >> 4))]);
        if chunk.len() > 1 {
            encoded.push(alphabet[usize::from(((second & 0x0f) << 2) | (third >> 6))]);
        } else if padded {
            encoded.push(b'=');
        }
        if chunk.len() > 2 {
            encoded.push(alphabet[usize::from(third & 0x3f)]);
        } else if padded {
            encoded.push(b'=');
        }
    }
    encoded
}

fn percent_encode(bytes: &[u8]) -> Vec<u8> {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = Vec::with_capacity(bytes.len());
    for byte in bytes {
        if byte.is_ascii_alphanumeric() || matches!(*byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(*byte);
        } else {
            encoded.extend_from_slice(&[
                b'%',
                HEX[usize::from(byte >> 4)],
                HEX[usize::from(byte & 0x0f)],
            ]);
        }
    }
    encoded
}
