//! Quoting-independent element identity and Unity-style scalar rendering.
//!
//! Unity writes identity scalars (fileID, guid, propertyPath, rid) as plain
//! text, while tools may quote them. Both spellings address the same element,
//! so identity uses decoded scalar text. Plain scalars hash exactly as the
//! parser fingerprint does, so identities of Unity-written data are unchanged.

use serde_json::Value;

use super::{Asset, Node, NodeKind, ScalarStyle};

/// Decoded scalar text; `None` for collections or undecodable scalars.
pub(crate) fn decoded_scalar(asset: &Asset, node: &Node) -> Option<String> {
    let NodeKind::Scalar(style) = node.kind else {
        return None;
    };
    let raw = asset.text(node.span);
    match style {
        ScalarStyle::Empty => Some(String::new()),
        ScalarStyle::Plain | ScalarStyle::Null if !raw.contains('\n') => Some(raw.trim().to_owned()),
        _ => serde_yaml::from_str::<String>(raw).ok(),
    }
}

fn scalar_hash(text: &str) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    let style = if text.is_empty() {
        ScalarStyle::Empty
    } else {
        ScalarStyle::Plain
    };
    hash.update(&[0, style as u8]);
    hash.update(text.as_bytes());
    *hash.finalize().as_bytes()
}

/// Structural hash where quoted scalars count as their decoded plain text.
pub(crate) fn identity_fingerprint(asset: &Asset, node: &Node) -> [u8; 32] {
    match &node.kind {
        NodeKind::Scalar(ScalarStyle::SingleQuoted | ScalarStyle::DoubleQuoted) => {
            decoded_scalar(asset, node)
                .map(|text| scalar_hash(&text))
                .unwrap_or(node.fingerprint)
        }
        NodeKind::Scalar(_) => node.fingerprint,
        NodeKind::Mapping(entries) => {
            let mut hash = blake3::Hasher::new();
            hash.update(&[1]);
            let mut sorted: Vec<_> = entries.iter().collect();
            sorted.sort_by(|a, b| a.key.cmp(&b.key));
            for entry in sorted {
                hash.update(&(entry.key.len() as u64).to_le_bytes());
                hash.update(entry.key.as_bytes());
                hash.update(&identity_fingerprint(asset, &entry.value));
            }
            *hash.finalize().as_bytes()
        }
        NodeKind::Sequence(items) => {
            let mut hash = blake3::Hasher::new();
            hash.update(&[2]);
            for item in items {
                hash.update(&identity_fingerprint(asset, &item.value));
            }
            *hash.finalize().as_bytes()
        }
    }
}

/// The same structural hash for a typed JSON value supplied by a caller.
pub(crate) fn json_identity_fingerprint(value: &Value) -> [u8; 32] {
    match value {
        Value::Object(map) => {
            let mut hash = blake3::Hasher::new();
            hash.update(&[1]);
            let mut keys: Vec<_> = map.keys().collect();
            keys.sort();
            for key in keys {
                hash.update(&(key.len() as u64).to_le_bytes());
                hash.update(key.as_bytes());
                hash.update(&json_identity_fingerprint(&map[key]));
            }
            *hash.finalize().as_bytes()
        }
        Value::Array(items) => {
            let mut hash = blake3::Hasher::new();
            hash.update(&[2]);
            for item in items {
                hash.update(&json_identity_fingerprint(item));
            }
            *hash.finalize().as_bytes()
        }
        Value::String(text) => scalar_hash(text),
        Value::Number(number) => scalar_hash(&number.to_string()),
        Value::Bool(value) => scalar_hash(if *value { "1" } else { "0" }),
        Value::Null => {
            let mut hash = blake3::Hasher::new();
            hash.update(&[0, ScalarStyle::Null as u8]);
            hash.update(b"null");
            *hash.finalize().as_bytes()
        }
    }
}

/// Identity text of a JSON scalar, including exact integer transport envelopes.
pub(crate) fn json_identity_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Object(map)
            if map.len() == 2
                && matches!(
                    map.get("kind").and_then(Value::as_str),
                    Some("int64" | "uint64")
                ) =>
        {
            map.get("value").and_then(Value::as_str).map(str::to_owned)
        }
        _ => None,
    }
}

pub(crate) fn json_pptr(value: &Value) -> bool {
    value.as_object().is_some_and(|map| {
        map.contains_key("fileID")
            && map
                .keys()
                .all(|key| matches!(key.as_str(), "fileID" | "guid" | "type"))
    })
}

pub(crate) fn is_packed_marker(value: &Value) -> bool {
    value
        .as_object()
        .is_some_and(|map| map.len() == 1 && map.contains_key("$locus_packed_array"))
}

/// Unity lists whose elements carry a unique identity, recognised by path.
/// `sequence_keys` (serialized nodes) and `json_sequence_keys` (caller JSON)
/// both derive element keys from this one table.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ListIdentity {
    /// `references/RefIds`: the managed-reference `rid`.
    Rid,
    /// PrefabInstance `m_Modifications`: target PPtr and propertyPath.
    Override,
    /// PrefabInstance `m_AddedGameObjects` / `m_AddedComponents`: `addedObject`.
    Added,
    /// PrefabInstance `m_RemovedComponents` / `m_RemovedGameObjects`: the PPtr.
    Removed,
    /// GameObject `m_Component`: the component PPtr.
    Component,
    /// Transform `m_Children`: the child PPtr.
    Child,
    /// SceneRoots `m_Roots`: the root Transform PPtr.
    Root,
}

impl ListIdentity {
    pub(crate) fn of(path: &str) -> Option<Self> {
        let modification = path.contains("/m_Modification/");
        Some(match path.rsplit('/').next()? {
            "RefIds" if path.ends_with("/references/RefIds") => Self::Rid,
            "m_Modifications" => Self::Override,
            "m_AddedGameObjects" | "m_AddedComponents" if modification => Self::Added,
            "m_RemovedComponents" | "m_RemovedGameObjects" if modification => Self::Removed,
            "m_Component" => Self::Component,
            "m_Children" => Self::Child,
            "m_Roots" if path.starts_with("/SceneRoots/") => Self::Root,
            _ => return None,
        })
    }

    /// Unity treats these lists as sets: element order carries no meaning, so
    /// a reordering never needs a decision. Components, children and scene
    /// roots keep their order.
    pub(crate) fn unordered(self) -> bool {
        matches!(self, Self::Rid | Self::Override | Self::Added | Self::Removed)
    }
}

/// Sequence identities for caller-supplied JSON, mirroring `sequence_keys`.
pub(crate) fn json_sequence_keys(path: &str, items: &[Value]) -> Option<Vec<String>> {
    let kind = ListIdentity::of(path)?;
    let mut keys = Vec::with_capacity(items.len());
    let mut seen = std::collections::HashSet::new();
    let file_id = |pptr: &Value| -> Option<String> {
        json_pptr(pptr).then_some(())?;
        json_identity_text(pptr.get("fileID")?)
    };
    for item in items {
        let key = match kind {
            ListIdentity::Rid => {
                let rid = json_identity_text(item.get("rid")?)?;
                rid.parse::<i64>().ok()?;
                format!("@rid={rid}")
            }
            ListIdentity::Override => {
                let target = item.get("target")?;
                if !json_pptr(target) {
                    return None;
                }
                let property = item.get("propertyPath")?.as_str()?;
                format!(
                    "@override={}:{}",
                    blake3::Hash::from_bytes(json_identity_fingerprint(target)).to_hex(),
                    property
                )
            }
            ListIdentity::Added => format!("@added={}", file_id(item.get("addedObject")?)?),
            ListIdentity::Removed => {
                let guid = item.get("guid").map(json_identity_text).unwrap_or(Some(String::new()))?;
                format!("@removed={guid}:{}", file_id(item)?)
            }
            ListIdentity::Component => format!("@component={}", file_id(item.get("component")?)?),
            ListIdentity::Child => format!("@child={}", file_id(item)?),
            ListIdentity::Root => format!("@root={}", file_id(item)?),
        };
        if !seen.insert(key.clone()) {
            return None;
        }
        keys.push(key);
    }
    Some(keys)
}

/// `{rid: N}` managed-reference pointer.
pub(crate) fn managed_reference(node: &Node) -> bool {
    node.entries()
        .is_some_and(|entries| entries.len() == 1 && entries[0].key == "rid")
}

/// Numeric reference identities (`fileID` of a PPtr, `rid` of a managed
/// reference) are typed: a JSON string never stands for a plain number there.
pub(crate) fn identity_scalar(parent: &Node, key: &str) -> bool {
    (key == "fileID" && super::validation::pptr(parent)) || (key == "rid" && managed_reference(parent))
}

/// Whether a serialized node already holds this typed JSON value. Plain
/// scalars compare by decoded text or numeric value so `007` stays `007`.
pub(crate) fn value_matches(asset: &Asset, node: &Node, value: &Value) -> bool {
    matches_value(asset, node, value, false)
}

pub(crate) fn matches_value(asset: &Asset, node: &Node, value: &Value, identity: bool) -> bool {
    if is_packed_marker(value) {
        return false;
    }
    match (&node.kind, value) {
        (NodeKind::Scalar(ScalarStyle::Plain), Value::String(_)) if identity => false,
        (NodeKind::Scalar(_), Value::String(text)) => {
            decoded_scalar(asset, node).is_some_and(|decoded| &decoded == text)
        }
        (NodeKind::Scalar(ScalarStyle::Plain), Value::Number(number)) => {
            numeric_equal(asset.text(node.span).trim(), number)
        }
        (NodeKind::Scalar(ScalarStyle::Plain), Value::Bool(flag)) => {
            let raw = asset.text(node.span).trim();
            if *flag {
                matches!(raw, "1" | "true" | "True" | "TRUE")
            } else {
                matches!(raw, "0" | "false" | "False" | "FALSE")
            }
        }
        (NodeKind::Scalar(ScalarStyle::Null | ScalarStyle::Empty), Value::Null) => true,
        (NodeKind::Mapping(entries), Value::Object(map)) => {
            entries.len() == map.len()
                && entries.iter().all(|entry| {
                    map.get(&entry.key).is_some_and(|value| {
                        matches_value(asset, &entry.value, value, identity_scalar(node, &entry.key))
                    })
                })
        }
        (NodeKind::Sequence(items), Value::Array(values)) => {
            items.len() == values.len()
                && items
                    .iter()
                    .zip(values)
                    .all(|(item, value)| matches_value(asset, &item.value, value, false))
        }
        _ => false,
    }
}

fn numeric_equal(raw: &str, number: &serde_json::Number) -> bool {
    if let Some(expected) = number.as_i64() {
        if let Ok(actual) = raw.parse::<i64>() {
            return actual == expected;
        }
    } else if let Some(expected) = number.as_u64() {
        if let Ok(actual) = raw.parse::<u64>() {
            return actual == expected;
        }
    }
    match (raw.parse::<f64>(), number.as_f64()) {
        (Ok(actual), Some(expected)) if actual.is_finite() => {
            // Integers above 2^53 must not compare equal through rounding.
            if number.is_f64() || actual.abs() <= 9_007_199_254_740_991.0 {
                actual == expected
            } else {
                false
            }
        }
        _ => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScalarClass {
    Empty,
    Null,
    Bool,
    Number,
    Text,
}

fn classify(text: &str) -> ScalarClass {
    if text.is_empty() {
        return ScalarClass::Empty;
    }
    if matches!(text, "null" | "Null" | "NULL" | "~") {
        return ScalarClass::Null;
    }
    if matches!(
        text,
        "true" | "True" | "TRUE" | "false" | "False" | "FALSE" | "yes" | "Yes" | "YES" | "no"
            | "No" | "NO" | "on" | "On" | "ON" | "off" | "Off" | "OFF" | "y" | "Y" | "n" | "N"
    ) {
        return ScalarClass::Bool;
    }
    let lower = text.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        ".nan" | ".inf" | "-.inf" | "+.inf" | "nan" | "inf" | "-inf" | "+inf" | "infinity"
            | "-infinity" | "+infinity"
    ) || text.parse::<f64>().is_ok()
        || lower
            .strip_prefix("0x")
            .or_else(|| lower.strip_prefix("-0x"))
            .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return ScalarClass::Number;
    }
    ScalarClass::Text
}

/// Conservative plain-scalar safety: ASCII, no indicators, no comment or
/// mapping separators, and no flow delimiters inside flow collections.
fn plain_safe(text: &str, flow: bool) -> bool {
    let Some(first) = text.bytes().next() else {
        return false;
    };
    if text != text.trim() || !text.bytes().all(|b| (0x20..0x7f).contains(&b)) {
        return false;
    }
    if b"?:,[]{}#&*!|>'\"%@`".contains(&first) {
        return false;
    }
    if first == b'-' && !text.as_bytes().get(1).is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'.' || *b == b'_') {
        return false;
    }
    if text.contains(": ") || text.contains(" #") || text.ends_with(':') {
        return false;
    }
    !(flow && text.bytes().any(|b| matches!(b, b',' | b'[' | b']' | b'{' | b'}')))
}

/// Double-quoted YAML. Unity escapes non-ASCII text as `\uXXXX`; callers ask
/// for that spelling when the replaced scalar already used it.
pub(crate) fn quoted(text: &str, unicode_escapes: bool) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02X}", c as u32))
            }
            c if unicode_escapes && !c.is_ascii() => {
                let code = c as u32;
                if code <= 0xFFFF {
                    out.push_str(&format!("\\u{code:04X}"));
                } else {
                    out.push_str(&format!("\\U{code:08X}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A string written where no existing scalar provides a spelling: plain when
/// that is unambiguous text, otherwise double-quoted.
pub(crate) fn new_string(text: &str, flow: bool) -> String {
    if classify(text) == ScalarClass::Text && plain_safe(text, flow) {
        text.to_owned()
    } else {
        quoted(text, false)
    }
}

/// Replace one scalar while keeping its lexical class: a plain number stays
/// plain when the new string is also a number, text stays unquoted text, and
/// anything that would change how a YAML reader types the value is quoted.
pub(crate) fn replacement_string(asset: &Asset, old: &Node, text: &str, flow: bool) -> String {
    let raw = asset.text(old.span);
    let old_plain = matches!(old.kind, NodeKind::Scalar(ScalarStyle::Plain | ScalarStyle::Empty));
    if old_plain && plain_safe(text, flow) {
        let old_class = classify(raw.trim());
        let new_class = classify(text);
        let compatible = match new_class {
            ScalarClass::Text => matches!(old_class, ScalarClass::Text | ScalarClass::Empty),
            ScalarClass::Empty => false,
            class => class == old_class,
        };
        if compatible {
            return text.to_owned();
        }
    }
    quoted(text, raw.contains("\\u") || raw.contains("\\U"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_and_quoted_identity_text_hash_identically() {
        let plain = super::super::parse(b"--- !u!1 &1\nA:\n  t: {fileID: 7, guid: abc, type: 3}\n").unwrap();
        let quoted = super::super::parse(b"--- !u!1 &1\nA:\n  t: {fileID: \"7\", guid: 'abc', type: 3}\n").unwrap();
        let node = |asset: &Asset| asset.documents[0].root.get("A").unwrap().get("t").unwrap().clone();
        let a = node(&plain);
        let b = node(&quoted);
        assert_eq!(identity_fingerprint(&plain, &a), a.fingerprint);
        assert_eq!(identity_fingerprint(&plain, &a), identity_fingerprint(&quoted, &b));
        assert_eq!(
            identity_fingerprint(&plain, &a),
            json_identity_fingerprint(&serde_json::json!({"fileID": 7, "guid": "abc", "type": 3}))
        );
    }

    #[test]
    fn replacement_keeps_lexical_class_and_quotes_ambiguous_text() {
        let asset = super::super::parse(b"--- !u!1 &1\nA:\n  n: 0.5\n  s: name\n  e:\n  u: \"\\u4E2D\"\n").unwrap();
        let body = asset.documents[0].root.get("A").unwrap();
        let n = body.get("n").unwrap();
        let s = body.get("s").unwrap();
        let e = body.get("e").unwrap();
        let u = body.get("u").unwrap();
        assert_eq!(replacement_string(&asset, n, "0.70710677", false), "0.70710677");
        assert_eq!(replacement_string(&asset, n, "abc", false), "\"abc\"");
        assert_eq!(replacement_string(&asset, s, "m_LocalRotation.y", false), "m_LocalRotation.y");
        assert_eq!(replacement_string(&asset, s, "data[3].x", true), "\"data[3].x\"");
        assert_eq!(replacement_string(&asset, s, "123", false), "\"123\"");
        assert_eq!(replacement_string(&asset, s, "true", false), "\"true\"");
        assert_eq!(replacement_string(&asset, s, "a: b", false), "\"a: b\"");
        assert_eq!(replacement_string(&asset, e, "filled", false), "filled");
        assert_eq!(replacement_string(&asset, u, "中文", false), "\"\\u4E2D\\u6587\"");
        assert_eq!(new_string("中文", false), "\"中文\"");
    }
}
