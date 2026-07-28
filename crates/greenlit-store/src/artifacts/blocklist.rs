//! The one non-JSON shape in this crate: Azure's block-list commit body.
//!
//! `@actions/artifact` uploads through `@azure/storage-blob`'s
//! `blockBlobClient.uploadStream(...)`, which stages each chunk under a
//! caller-chosen block id and then commits an *ordering* — the blocks are
//! assembled in the order this document lists them, which is not necessarily
//! the order they arrived in. Getting that wrong produces a corrupt artifact
//! that still uploads and finalizes cleanly, so the order is parsed rather
//! than assumed.
//!
//! The document looks like:
//!
//! ```xml
//! <?xml version="1.0" encoding="utf-8"?>
//! <BlockList>
//!   <Latest>YmxvY2stMDAwMA==</Latest>
//!   <Latest>YmxvY2stMDAwMQ==</Latest>
//! </BlockList>
//! ```
//!
//! Azure defines three element names — `Committed`, `Uncommitted`, and
//! `Latest` — which differ only in *which staged copy* of a repeated id to
//! prefer. A shim that has never committed a block before sees only one copy
//! of each id, so all three select the same bytes and are treated alike. What
//! matters is the sequence.
//!
//! This is a deliberately small reader for exactly that document, not a
//! general XML parser: it recognizes the three element names and ignores
//! everything else, so a declaration, comment, or attribute cannot change the
//! result.

/// The block ids to assemble, in order.
///
/// Returns an empty list for a body containing no block elements, which the
/// caller treats as a bad request rather than as an empty artifact.
#[must_use]
pub fn parse(body: &str) -> Vec<String> {
    const ELEMENTS: [&str; 3] = ["Latest", "Committed", "Uncommitted"];

    let mut ids = Vec::new();
    let mut rest = body;
    while let Some(open) = rest.find('<') {
        let after_open = &rest[open + 1..];
        let Some(close) = after_open.find('>') else {
            break;
        };
        let tag = &after_open[..close];
        let body_start = &after_open[close + 1..];

        // Only an opening tag whose name is one of the three carries an id.
        let name = tag.split_whitespace().next().unwrap_or(tag);
        if ELEMENTS.contains(&name)
            && let Some(end) = body_start.find('<')
        {
            let id = body_start[..end].trim();
            if !id.is_empty() {
                ids.push(id.to_string());
            }
        }
        rest = body_start;
    }
    ids
}
