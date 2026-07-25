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

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn the_listed_order_is_preserved() {
        let body = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
             <BlockList><Latest>YQ==</Latest><Latest>Yg==</Latest><Latest>Yw==</Latest></BlockList>";
        assert_eq!(parse(body), vec!["YQ==", "Yg==", "Yw=="]);
    }

    #[test]
    fn out_of_arrival_order_ids_are_taken_as_written() {
        // The upload is concurrent, so the commit order is the only source of
        // truth for how the blocks assemble.
        let body = "<BlockList><Latest>third</Latest><Latest>first</Latest></BlockList>";
        assert_eq!(parse(body), vec!["third", "first"]);
    }

    #[test]
    fn all_three_element_names_are_accepted() {
        let body = "<BlockList><Committed>a</Committed><Uncommitted>b</Uncommitted>\
             <Latest>c</Latest></BlockList>";
        assert_eq!(parse(body), vec!["a", "b", "c"]);
    }

    #[test]
    fn declarations_whitespace_and_attributes_do_not_contribute_ids() {
        let body = "<?xml version=\"1.0\"?>\n<BlockList>\n  <Latest>only</Latest>\n</BlockList>\n";
        assert_eq!(parse(body), vec!["only"]);
    }

    #[test]
    fn a_body_with_no_blocks_yields_nothing() {
        assert!(parse("<BlockList></BlockList>").is_empty());
        assert!(parse("").is_empty());
        assert!(parse("not xml at all").is_empty());
    }
}
