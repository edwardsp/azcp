use std::fmt::Write;

use azcp::BlobItem;

pub fn serialize(entries: &[BlobItem]) -> String {
    let mut out = String::with_capacity(entries.len() * 80);
    for e in entries {
        let size = e
            .properties
            .as_ref()
            .and_then(|p| p.content_length)
            .unwrap_or(0);
        let lm = e
            .properties
            .as_ref()
            .and_then(|p| p.last_modified.as_deref())
            .unwrap_or("-");
        let _ = writeln!(out, "{}\t{}\t{}", e.name, size, lm);
    }
    out
}

pub fn deserialize(text: &str) -> Vec<BlobItem> {
    use azcp::BlobProperties;
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.trim_end_matches('\r');
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split('\t');
        let Some(name) = fields.next().filter(|s| !s.is_empty()) else {
            continue;
        };
        let size = fields.next().and_then(|s| s.parse::<u64>().ok());
        let Some(size) = size else { continue };
        let lm = fields.next().and_then(|s| {
            let s = s.trim();
            if s.is_empty() || s == "-" {
                None
            } else {
                Some(s.to_string())
            }
        });
        out.push(BlobItem {
            name: name.to_string(),
            properties: Some(BlobProperties {
                last_modified: lm,
                content_length: Some(size),
                content_type: None,
                content_md5: None,
                etag: None,
                blob_type: None,
                access_tier: None,
            }),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use azcp::BlobProperties;

    fn item(name: &str, size: u64, lm: Option<&str>) -> BlobItem {
        BlobItem {
            name: name.to_string(),
            properties: Some(BlobProperties {
                last_modified: lm.map(str::to_string),
                content_length: Some(size),
                content_type: None,
                content_md5: None,
                etag: None,
                blob_type: None,
                access_tier: None,
            }),
        }
    }

    #[test]
    fn roundtrip() {
        let original = vec![
            item("a/b.bin", 1024, Some("2024-01-01T00:00:00Z")),
            item("c.txt", 0, None),
        ];
        let text = serialize(&original);
        let back = deserialize(&text);
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].name, "a/b.bin");
        assert_eq!(
            back[0].properties.as_ref().unwrap().content_length,
            Some(1024)
        );
        assert_eq!(
            back[0]
                .properties
                .as_ref()
                .unwrap()
                .last_modified
                .as_deref(),
            Some("2024-01-01T00:00:00Z")
        );
        assert_eq!(back[1].name, "c.txt");
        assert_eq!(back[1].properties.as_ref().unwrap().last_modified, None);
    }

    #[test]
    fn skips_blank_and_comment_lines() {
        let text = "# header\n\nfoo\t10\t-\n";
        let entries = deserialize(text);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "foo");
    }
}
