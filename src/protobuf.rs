use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

/// Decodes a Protobuf base-128 varint from `buf`, returning `(value, bytes_consumed)`.
pub fn decode_varint(buf: &[u8]) -> Result<(u64, usize)> {
    let mut result: u64 = 0;
    let mut shift: u32 = 0;
    for (i, &byte) in buf.iter().enumerate() {
        if shift >= 64 {
            bail!("Protobuf varint overflow");
        }
        result |= ((byte & 0x7F) as u64) << shift;
        if (byte & 0x80) == 0 {
            return Ok((result, i + 1));
        }
        shift += 7;
    }
    bail!("Unexpected EOF while decoding Protobuf varint");
}

/// Appends a Protobuf base-128 varint to `out`.
pub fn encode_varint(mut val: u64, out: &mut Vec<u8>) {
    loop {
        if (val & !0x7F) == 0 {
            out.push(val as u8);
            break;
        } else {
            out.push(((val & 0x7F) as u8) | 0x80);
            val >>= 7;
        }
    }
}

/// Validates whether `buf` is a syntactically well-formed Protobuf message containing at least one field,
/// where every wire tag, varint, fixed-width value, and length-delimited payload fits cleanly inside `buf`.
pub fn is_likely_protobuf_message(buf: &[u8]) -> bool {
    if buf.is_empty() {
        return false;
    }
    let mut pos = 0;
    let mut field_count = 0;
    while pos < buf.len() {
        let Ok((tag, tag_len)) = decode_varint(&buf[pos..]) else {
            return false;
        };
        let field_num = tag >> 3;
        let wire_type = tag & 0x07;
        if field_num == 0 || field_num > 536_870_911 {
            return false;
        }
        pos += tag_len;
        match wire_type {
            0 => {
                // Varint
                let Ok((_, v_len)) = decode_varint(&buf[pos..]) else {
                    return false;
                };
                pos += v_len;
            }
            1 => {
                // 64-bit
                if pos + 8 > buf.len() {
                    return false;
                }
                pos += 8;
            }
            2 => {
                // Length-delimited
                let Ok((len, l_len)) = decode_varint(&buf[pos..]) else {
                    return false;
                };
                pos += l_len;
                let len_usize = len as usize;
                if pos.checked_add(len_usize).is_none_or(|end| end > buf.len()) {
                    return false;
                }
                pos += len_usize;
            }
            5 => {
                // 32-bit
                if pos + 4 > buf.len() {
                    return false;
                }
                pos += 4;
            }
            _ => return false,
        }
        field_count += 1;
    }
    field_count > 0 && pos == buf.len()
}

/// Recursively walks a Protobuf wire buffer and replaces occurrences of `from_bytes` with `to_bytes`
/// inside UTF-8 string payloads, automatically recalculating all parent length-delimited varints!
///
/// This is essential when remapping `$HOME` paths across machines (`from_home` <-> `to_home`)
/// inside `conversation_summaries.raw_summary`, `*_summaries_proto.pb`, `trajectory_metadata_blob`,
/// and `implicit/<id>.pb`, because the two `$HOME` strings have different byte lengths.
pub fn rewrite_protobuf_strings(buf: &[u8], from_str: &str, to_str: &str) -> Result<Vec<u8>> {
    if from_str.is_empty() || from_str == to_str {
        return Ok(buf.to_vec());
    }
    if !buf
        .windows(from_str.len())
        .any(|w| w == from_str.as_bytes())
    {
        return Ok(buf.to_vec());
    }

    let mut out = Vec::with_capacity(buf.len() + 64);
    let mut pos = 0;

    while pos < buf.len() {
        let (tag, tag_len) = decode_varint(&buf[pos..])?;
        let wire_type = tag & 0x07;
        out.extend_from_slice(&buf[pos..pos + tag_len]);
        pos += tag_len;

        match wire_type {
            0 => {
                let (_, v_len) = decode_varint(&buf[pos..])?;
                out.extend_from_slice(&buf[pos..pos + v_len]);
                pos += v_len;
            }
            1 => {
                if pos + 8 > buf.len() {
                    bail!("Unexpected EOF in 64-bit Protobuf field");
                }
                out.extend_from_slice(&buf[pos..pos + 8]);
                pos += 8;
            }
            2 => {
                let (len, l_len) = decode_varint(&buf[pos..])?;
                pos += l_len;
                let len_usize = len as usize;
                if pos + len_usize > buf.len() {
                    bail!("Unexpected EOF in length-delimited Protobuf field");
                }
                let payload = &buf[pos..pos + len_usize];
                pos += len_usize;

                let new_payload = if !payload
                    .windows(from_str.len())
                    .any(|w| w == from_str.as_bytes())
                {
                    payload.to_vec()
                } else if let Ok(utf8_str) = std::str::from_utf8(payload) {
                    // Check if it's a plain UTF-8 string (no control chars besides \n, \r, \t)
                    let is_plain_text = utf8_str
                        .chars()
                        .all(|c| !c.is_control() || matches!(c, '\n' | '\r' | '\t'));
                    if is_plain_text {
                        utf8_str.replace(from_str, to_str).into_bytes()
                    } else if is_likely_protobuf_message(payload) {
                        rewrite_protobuf_strings(payload, from_str, to_str)
                            .unwrap_or_else(|_| payload.to_vec())
                    } else {
                        payload.to_vec()
                    }
                } else if is_likely_protobuf_message(payload) {
                    rewrite_protobuf_strings(payload, from_str, to_str)
                        .unwrap_or_else(|_| payload.to_vec())
                } else {
                    payload.to_vec()
                };

                encode_varint(new_payload.len() as u64, &mut out);
                out.extend_from_slice(&new_payload);
            }
            5 => {
                if pos + 4 > buf.len() {
                    bail!("Unexpected EOF in 32-bit Protobuf field");
                }
                out.extend_from_slice(&buf[pos..pos + 4]);
                pos += 4;
            }
            other => {
                bail!("Unsupported Protobuf wire type: {}", other);
            }
        }
    }

    Ok(out)
}

/// Represents the low-level wire-type value of a Protobuf field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireValue {
    Varint(u64),
    Fixed64([u8; 8]),
    LengthDelimited(Vec<u8>),
    Fixed32([u8; 4]),
}

/// A parsed Protobuf field preserving wire tag and raw payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawField {
    pub tag: u64,
    pub wire_type: u32,
    pub value: WireValue,
}

/// Decodes a flat list of Protobuf fields from `buf`.
pub fn decode_raw_fields(buf: &[u8]) -> Result<Vec<RawField>> {
    let mut fields = Vec::new();
    let mut pos = 0;
    while pos < buf.len() {
        let (tag_wire, tag_len) = decode_varint(&buf[pos..])?;
        pos += tag_len;
        let wire_type = (tag_wire & 0x07) as u32;
        let field_num = tag_wire >> 3;
        match wire_type {
            0 => {
                let (val, v_len) = decode_varint(&buf[pos..])?;
                pos += v_len;
                fields.push(RawField {
                    tag: field_num,
                    wire_type,
                    value: WireValue::Varint(val),
                });
            }
            1 => {
                if pos + 8 > buf.len() {
                    bail!("Unexpected EOF in 64-bit field");
                }
                let mut bytes = [0u8; 8];
                bytes.copy_from_slice(&buf[pos..pos + 8]);
                pos += 8;
                fields.push(RawField {
                    tag: field_num,
                    wire_type,
                    value: WireValue::Fixed64(bytes),
                });
            }
            2 => {
                let (len, l_len) = decode_varint(&buf[pos..])?;
                pos += l_len;
                let len_usize = len as usize;
                if pos + len_usize > buf.len() {
                    bail!("Unexpected EOF in length-delimited field");
                }
                let payload = buf[pos..pos + len_usize].to_vec();
                pos += len_usize;
                fields.push(RawField {
                    tag: field_num,
                    wire_type,
                    value: WireValue::LengthDelimited(payload),
                });
            }
            5 => {
                if pos + 4 > buf.len() {
                    bail!("Unexpected EOF in 32-bit field");
                }
                let mut bytes = [0u8; 4];
                bytes.copy_from_slice(&buf[pos..pos + 4]);
                pos += 4;
                fields.push(RawField {
                    tag: field_num,
                    wire_type,
                    value: WireValue::Fixed32(bytes),
                });
            }
            other => bail!("Unsupported wire type: {}", other),
        }
    }
    Ok(fields)
}

/// Encodes a list of `RawField`s back into wire format bytes.
pub fn encode_raw_fields(fields: &[RawField]) -> Vec<u8> {
    let mut out = Vec::new();
    for f in fields {
        let tag_wire = (f.tag << 3) | ((f.wire_type as u64) & 0x07);
        encode_varint(tag_wire, &mut out);
        match &f.value {
            WireValue::Varint(v) => encode_varint(*v, &mut out),
            WireValue::Fixed64(bytes) => out.extend_from_slice(bytes),
            WireValue::LengthDelimited(bytes) => {
                encode_varint(bytes.len() as u64, &mut out);
                out.extend_from_slice(bytes);
            }
            WireValue::Fixed32(bytes) => out.extend_from_slice(bytes),
        }
    }
    out
}

/// Updates or inserts Field 18 (project_id string) inside Field 17 (context submessage)
/// of a `TrajectorySummary` Protobuf payload, recalculating parent varint length delimiters.
///
/// This ensures sessions appear nested under the workspace in Jetski Web / Agy IDE rather
/// than being hidden or relegated to `default-cli-project`.
pub fn set_summary_project_id(buf: &[u8], project_id: &str) -> Result<Vec<u8>> {
    if buf.is_empty() || project_id.is_empty() {
        return Ok(buf.to_vec());
    }
    let mut top_fields = match decode_raw_fields(buf) {
        Ok(f) => f,
        Err(_) => return Ok(buf.to_vec()),
    };

    let mut found_context = false;
    for f in &mut top_fields {
        if f.tag == 17 && f.wire_type == 2 {
            if let WireValue::LengthDelimited(ref sub_payload) = f.value {
                if let Ok(mut sub_fields) = decode_raw_fields(sub_payload) {
                    let mut found_p_id = false;
                    for sf in &mut sub_fields {
                        if sf.tag == 18 && sf.wire_type == 2 {
                            sf.value = WireValue::LengthDelimited(project_id.as_bytes().to_vec());
                            found_p_id = true;
                            break;
                        }
                    }
                    if !found_p_id {
                        sub_fields.push(RawField {
                            tag: 18,
                            wire_type: 2,
                            value: WireValue::LengthDelimited(project_id.as_bytes().to_vec()),
                        });
                    }
                    f.value = WireValue::LengthDelimited(encode_raw_fields(&sub_fields));
                    found_context = true;
                }
            }
        }
    }

    if !found_context {
        let sub_fields = vec![RawField {
            tag: 18,
            wire_type: 2,
            value: WireValue::LengthDelimited(project_id.as_bytes().to_vec()),
        }];
        top_fields.push(RawField {
            tag: 17,
            wire_type: 2,
            value: WireValue::LengthDelimited(encode_raw_fields(&sub_fields)),
        });
    }

    Ok(encode_raw_fields(&top_fields))
}

/// Extracts all printable ASCII substrings of length >= `min_len` from a binary buffer.
pub fn extract_printable_strings(buf: &[u8], min_len: usize) -> Vec<String> {
    let mut results = Vec::new();
    let mut current = Vec::new();
    for &b in buf {
        if (0x20..=0x7E).contains(&b) {
            current.push(b);
        } else {
            if current.len() >= min_len {
                if let Ok(s) = String::from_utf8(current.clone()) {
                    results.push(s);
                }
            }
            current.clear();
        }
    }
    if current.len() >= min_len {
        if let Ok(s) = String::from_utf8(current) {
            results.push(s);
        }
    }
    results
}

/// Represents the parsed `map<string, TrajectorySummary>` stored in `*_summaries_proto.pb`
/// inside a `~/.gemini/<harness>/` directory.
///
/// Wire format:
/// - Repeated field 1 (`tag = 0x0a`, wire type 2 = length-delimited): Map Entry message
///   - Field 1 (`tag = 0x0a`, wire type 2): `key` (`conversation_id` UTF-8 string)
///   - Field 2 (`tag = 0x12`, wire type 2): `value` (`TrajectorySummary` message bytes — identical to `conversation_summaries.raw_summary` in SQLite!)
/// - Additional top-level fields (such as `tag = 0x12` in `agyhub_summaries_proto.pb`) are preserved
///   losslessly in `extra_top_fields`.
#[derive(Debug, Clone, Default)]
pub struct ProtobufSummariesMap {
    entries: BTreeMap<String, Vec<u8>>,
    extra_top_fields: Vec<(u64, Vec<u8>)>,
}

impl ProtobufSummariesMap {
    pub fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
            extra_top_fields: Vec::new(),
        }
    }

    /// Parses a `*_summaries_proto.pb` file from disk. If the file does not exist, returns an empty map.
    pub fn load_from_path(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::new());
        }
        let bytes =
            fs::read(path).with_context(|| format!("Failed to read {}", path.display()))?;
        Self::from_bytes(&bytes)
    }

    /// Parses `*_summaries_proto.pb` from raw bytes.
    pub fn from_bytes(buf: &[u8]) -> Result<Self> {
        let mut entries = BTreeMap::new();
        let mut extra_top_fields = Vec::new();
        let mut pos = 0;

        while pos < buf.len() {
            let (tag, tag_len) = decode_varint(&buf[pos..])?;
            pos += tag_len;
            let wire_type = tag & 0x07;

            if tag == 0x0a {
                let (entry_len, l_len) = decode_varint(&buf[pos..])?;
                pos += l_len;
                let entry_end = pos + (entry_len as usize);
                if entry_end > buf.len() {
                    bail!("Truncated map entry in *_summaries_proto.pb");
                }
                let entry_slice = &buf[pos..entry_end];
                pos = entry_end;

                let (key, val) = parse_map_entry(entry_slice)?;
                if !key.is_empty() {
                    entries.insert(key, val);
                }
            } else {
                // Skip or preserve other top-level fields losslessly
                match wire_type {
                    0 => {
                        let (_, v_len) = decode_varint(&buf[pos..])?;
                        pos += v_len;
                    }
                    1 => pos += 8,
                    2 => {
                        let (l, l_len) = decode_varint(&buf[pos..])?;
                        pos += l_len;
                        let end = pos + (l as usize);
                        if end > buf.len() {
                            bail!("Truncated top-level field in *_summaries_proto.pb");
                        }
                        extra_top_fields.push((tag, buf[pos..end].to_vec()));
                        pos = end;
                    }
                    5 => pos += 4,
                    other => bail!(
                        "Unsupported wire type {} in *_summaries_proto.pb",
                        other
                    ),
                }
            }
        }

        Ok(Self {
            entries,
            extra_top_fields,
        })
    }

    /// Serializes the map back to valid `*_summaries_proto.pb` wire bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for (key, val_bytes) in &self.entries {
            let mut entry_buf =
                Vec::with_capacity(2 + key.len() + 5 + val_bytes.len());
            // Field 1: key (string)
            encode_varint(0x0a, &mut entry_buf);
            encode_varint(key.len() as u64, &mut entry_buf);
            entry_buf.extend_from_slice(key.as_bytes());
            // Field 2: value (TrajectorySummary message)
            encode_varint(0x12, &mut entry_buf);
            encode_varint(val_bytes.len() as u64, &mut entry_buf);
            entry_buf.extend_from_slice(val_bytes);

            // Outer field 1: map entry
            encode_varint(0x0a, &mut out);
            encode_varint(entry_buf.len() as u64, &mut out);
            out.extend_from_slice(&entry_buf);
        }
        for (tag, payload) in &self.extra_top_fields {
            encode_varint(*tag, &mut out);
            encode_varint(payload.len() as u64, &mut out);
            out.extend_from_slice(payload);
        }
        out
    }

    /// Writes the serialized `*_summaries_proto.pb` atomically to `path`.
    pub fn save_to_path(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp_path = path.with_extension("pb.tmp");
        fs::write(&tmp_path, self.to_bytes())
            .with_context(|| format!("Failed to write temp file {}", tmp_path.display()))?;
        fs::rename(&tmp_path, path).with_context(|| {
            format!(
                "Failed to rename {} to {}",
                tmp_path.display(),
                path.display()
            )
        })?;
        Ok(())
    }

    pub fn get(&self, conversation_id: &str) -> Option<&[u8]> {
        self.entries.get(conversation_id).map(|v| v.as_slice())
    }

    pub fn upsert(&mut self, conversation_id: String, raw_summary: Vec<u8>) {
        self.entries.insert(conversation_id, raw_summary);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(&self) -> &BTreeMap<String, Vec<u8>> {
        &self.entries
    }
}

/// Extracts workspace URIs (`file://...`) and optional title from a `TrajectorySummary` protobuf payload.
pub fn extract_summary_metadata_from_pb(raw_summary: &[u8]) -> ( Option<String>, Vec<String> ) {
    let strings = extract_printable_strings(raw_summary, 4);
    let mut uris = Vec::new();
    let mut title: Option<String> = None;

    for s in strings {
        if let Some(idx) = s.find("file:///") {
            let candidate = &s[idx..];
            let end = candidate
                .find(|c: char| matches!(c, '"' | '\'' | ' ' | '\n' | '\r' | '\t' | ')' | ']'))
                .unwrap_or(candidate.len());
            let clean_uri = candidate[..end].trim_end_matches('/').to_string();
            if !uris.contains(&clean_uri) {
                uris.push(clean_uri);
            }
        } else if title.is_none()
            && s.len() >= 5
            && s.len() <= 120
            && !s.starts_with('/')
            && !s.contains("http://")
            && !s.contains("https://")
            && s.chars().any(|c| c.is_ascii_alphabetic())
        {
            title = Some(s);
        }
    }
    (title, uris)
}

fn parse_map_entry(buf: &[u8]) -> Result<(String, Vec<u8>)> {
    let mut key = String::new();
    let mut val = Vec::new();
    let mut pos = 0;

    while pos < buf.len() {
        let (tag, tag_len) = decode_varint(&buf[pos..])?;
        pos += tag_len;
        let wire_type = tag & 0x07;

        match (tag, wire_type) {
            (0x0a, 2) => {
                let (k_len, l_len) = decode_varint(&buf[pos..])?;
                pos += l_len;
                let k_end = pos + (k_len as usize);
                if k_end > buf.len() {
                    bail!("Truncated key in map entry");
                }
                key = String::from_utf8_lossy(&buf[pos..k_end]).into_owned();
                pos = k_end;
            }
            (0x12, 2) => {
                let (v_len, l_len) = decode_varint(&buf[pos..])?;
                pos += l_len;
                let v_end = pos + (v_len as usize);
                if v_end > buf.len() {
                    bail!("Truncated value in map entry");
                }
                val = buf[pos..v_end].to_vec();
                pos = v_end;
            }
            (_, 0) => {
                let (_, v_len) = decode_varint(&buf[pos..])?;
                pos += v_len;
            }
            (_, 1) => pos += 8,
            (_, 2) => {
                let (l, l_len) = decode_varint(&buf[pos..])?;
                pos += l_len + (l as usize);
            }
            (_, 5) => pos += 4,
            (_, other) => bail!("Unsupported wire type {} in map entry", other),
        }
    }

    Ok((key, val))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_varint_roundtrip() {
        for &v in &[0u64, 1, 127, 128, 300, 16384, 1_000_000, u64::MAX >> 1] {
            let mut buf = Vec::new();
            encode_varint(v, &mut buf);
            let (decoded, consumed) = decode_varint(&buf).unwrap();
            assert_eq!(decoded, v);
            assert_eq!(consumed, buf.len());
        }
    }

    #[test]
    fn test_nested_protobuf_rewrite_across_varint_boundary() {
        let mac_uri = "file:///Users/devuser/Documents/projects/sample-sync-engine/"
            .to_string()
            + &"a".repeat(80);
        let linux_uri = mac_uri.replace("/Users/devuser", "/home/remote/devuser");

        // Inner message: field 1 (tag 0x0a) = mac_uri
        let mut inner = Vec::new();
        encode_varint(0x0a, &mut inner);
        encode_varint(mac_uri.len() as u64, &mut inner);
        inner.extend_from_slice(mac_uri.as_bytes());

        // Outer message: field 2 (tag 0x12) = inner, field 3 (tag 0x18, varint) = 42
        let mut outer = Vec::new();
        encode_varint(0x12, &mut outer);
        encode_varint(inner.len() as u64, &mut outer);
        outer.extend_from_slice(&inner);
        encode_varint(0x18, &mut outer);
        encode_varint(42, &mut outer);

        let rewritten =
            rewrite_protobuf_strings(&outer, "/Users/devuser", "/home/remote/devuser").unwrap();
        assert!(is_likely_protobuf_message(&rewritten));

        let strings = extract_printable_strings(&rewritten, 10);
        assert_eq!(strings, vec![linux_uri]);

        let restored =
            rewrite_protobuf_strings(&rewritten, "/home/remote/devuser", "/Users/devuser").unwrap();
        assert_eq!(restored, outer);
    }

    #[test]
    fn test_protobuf_summaries_map_roundtrip() {
        let mut map = ProtobufSummariesMap::new();
        map.upsert(
            "11111111-2222-3333-4444-555555555555".to_string(),
            vec![0x0a, 0x05, b'h', b'e', b'l', b'l', b'o'],
        );
        map.upsert(
            "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_string(),
            vec![0x0a, 0x05, b'w', b'o', b'r', b'l', b'd'],
        );
        map.extra_top_fields
            .push((0x12, vec![0x08, 0x01, 0x12, 0x02, b'o', b'k']));

        let bytes = map.to_bytes();
        let parsed = ProtobufSummariesMap::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(
            parsed.get("11111111-2222-3333-4444-555555555555").unwrap(),
            &[0x0a, 0x05, b'h', b'e', b'l', b'l', b'o']
        );
        assert_eq!(parsed.extra_top_fields.len(), 1);
    }

    #[test]
    fn test_set_summary_project_id_roundtrip() {
        // Build a mock TrajectorySummary:
        // Top field 1: title (string)
        // Top field 17: context submessage
        //   Sub field 1: folderUri (string)
        //   Sub field 18: project_id (string: "default-cli-project")
        let mut sub = Vec::new();
        let uri = b"file:///proj/abc";
        encode_varint(0x0a, &mut sub);
        encode_varint(uri.len() as u64, &mut sub);
        sub.extend_from_slice(uri);
        let cli_proj = b"default-cli-project";
        encode_varint((18 << 3) | 2, &mut sub);
        encode_varint(cli_proj.len() as u64, &mut sub);
        sub.extend_from_slice(cli_proj);

        let mut top = Vec::new();
        encode_varint(0x0a, &mut top);
        encode_varint(5, &mut top);
        top.extend_from_slice(b"Title");
        encode_varint((17 << 3) | 2, &mut top);
        encode_varint(sub.len() as u64, &mut top);
        top.extend_from_slice(&sub);

        let target_uuid = "79ad68c4-ecb9-4454-ac43-bc6c9f852a01";
        let updated = set_summary_project_id(&top, target_uuid).unwrap();
        assert!(is_likely_protobuf_message(&updated));

        // Ensure target_uuid is present and default-cli-project is replaced
        assert!(updated
            .windows(target_uuid.len())
            .any(|w| w == target_uuid.as_bytes()));
        assert!(!updated
            .windows(cli_proj.len())
            .any(|w| w == cli_proj));
        assert!(updated.windows(uri.len()).any(|w| w == uri));

        // Also test inserting project_id into a message without field 17
        let mut top_no_ctx = Vec::new();
        encode_varint(0x0a, &mut top_no_ctx);
        encode_varint(5, &mut top_no_ctx);
        top_no_ctx.extend_from_slice(b"Title");
        let inserted = set_summary_project_id(&top_no_ctx, target_uuid).unwrap();
        assert!(is_likely_protobuf_message(&inserted));
        assert!(inserted
            .windows(target_uuid.len())
            .any(|w| w == target_uuid.as_bytes()));
    }
}
