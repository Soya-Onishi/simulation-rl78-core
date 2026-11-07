//! Deterministic id hashing and public hex encoding for wire payloads.

use std::collections::HashMap;

use xxhash_rust::xxh3::xxh3_64;

/// Stable 64-bit hash of a board id / path string (UTF-8 bytes).
#[must_use]
pub fn board_id_hash(board_id: &str) -> u64 {
    sim_kernel::id_hash(board_id)
}

/// Stable 64-bit hash of a port / endpoint name (UTF-8 bytes).
#[must_use]
pub fn port_id_hash(name: &str) -> u64 {
    sim_kernel::id_hash(name)
}

/// Stable 64-bit hash of a probe name (UTF-8 bytes).
#[must_use]
pub fn probe_name_hash(name: &str) -> u64 {
    sim_kernel::id_hash(name)
}

/// Cluster-wide probe id: `xxh3(board_id || 0x00 || name)`.
#[must_use]
pub fn probe_id_hash(board_id: &str, name: &str) -> u64 {
    let mut bytes = Vec::with_capacity(board_id.len() + 1 + name.len());
    bytes.extend_from_slice(board_id.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(name.as_bytes());
    xxh3_64(&bytes)
}

/// Logical edge id string, then `xxh3_64` (same form as historical `names::edge_id`).
#[must_use]
pub fn edge_id_hash(from_board: &str, from_ep: &str, to_board: &str, to_ep: &str) -> u64 {
    let logical = format!("{from_board}:{from_ep}->{to_board}:{to_ep}");
    xxh3_64(logical.as_bytes())
}

/// Format a u64 id as a 16-digit lowercase hex string (no `0x` prefix).
#[must_use]
pub fn format_id_hex(id: u64) -> String {
    format!("{id:016x}")
}

/// Parse a public API id: exactly 16 lowercase hex digits.
#[must_use]
pub fn parse_id_hex(text: &str) -> Option<u64> {
    if text.len() != 16 || !text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    u64::from_str_radix(text, 16).ok()
}

/// Build reverse lookup for unique id strings.
///
/// The same string may appear more than once. Distinct strings that share a
/// hash are an error.
pub fn id_hash_table<'a>(
    hash: impl Fn(&str) -> u64,
    ids: impl IntoIterator<Item = &'a str>,
) -> Result<HashMap<u64, String>, String> {
    let mut table = HashMap::new();
    for id in ids {
        let h = hash(id);

        if let Some(existing) = table.insert(h, id.to_string()) && existing != id {
            return Err(format!(
                "hash collision between '{existing}' and '{id}' (hash={h:#x})"
            ));
        }
    }
    Ok(table)
}

/// Build reverse lookup; errors if two distinct board ids collide.
pub fn board_hash_table<'a>(
    board_ids: impl IntoIterator<Item = &'a str>,
) -> Result<HashMap<u64, String>, String> {
    id_hash_table(board_id_hash, board_ids)
}

/// Build reverse lookup for path strings (`boards[].id` and edge route paths).
pub fn path_hash_table<'a>(
    paths: impl IntoIterator<Item = &'a str>,
) -> Result<HashMap<u64, String>, String> {
    id_hash_table(board_id_hash, paths)
}

/// Build reverse lookup for port / endpoint name strings.
pub fn port_hash_table<'a>(
    ports: impl IntoIterator<Item = &'a str>,
) -> Result<HashMap<u64, String>, String> {
    id_hash_table(port_id_hash, ports)
}

/// Build reverse lookup for logical edge-id strings.
pub fn edge_hash_table<'a>(
    edge_ids: impl IntoIterator<Item = &'a str>,
) -> Result<HashMap<u64, String>, String> {
    id_hash_table(|s| xxh3_64(s.as_bytes()), edge_ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable() {
        assert_eq!(board_id_hash("a"), board_id_hash("a"));
        assert_ne!(board_id_hash("a"), board_id_hash("b"));
        assert_eq!(port_id_hash("tx"), board_id_hash("tx"));
    }

    #[test]
    fn probe_id_includes_board_and_name() {
        assert_eq!(probe_id_hash("a", "vcc"), probe_id_hash("a", "vcc"));
        assert_ne!(probe_id_hash("a", "vcc"), probe_id_hash("b", "vcc"));
        assert_ne!(probe_id_hash("a", "vcc"), probe_id_hash("a", "gnd"));
        // NUL separator avoids `ab`+`c` vs `a`+`bc` ambiguity.
        assert_ne!(probe_id_hash("ab", "c"), probe_id_hash("a", "bc"));
    }

    #[test]
    fn table_allows_same_string_twice() {
        let table = board_hash_table(["a", "a"]).unwrap();
        assert_eq!(table.len(), 1);
        assert_eq!(table.get(&board_id_hash("a")).map(String::as_str), Some("a"));
    }

    #[test]
    fn hex_round_trip() {
        let h = board_id_hash("sys/left/a");
        let text = format_id_hex(h);
        assert_eq!(text.len(), 16);
        assert_eq!(parse_id_hex(&text), Some(h));
        assert_eq!(parse_id_hex(&text.to_uppercase()), None);
        assert_eq!(parse_id_hex(&format!("0x{text}")), None);
        assert_eq!(parse_id_hex("sys/left/a"), None);
    }

    #[test]
    fn edge_id_hash_matches_logical_xxh3() {
        let logical = "a:tx->b:rx";
        assert_eq!(edge_id_hash("a", "tx", "b", "rx"), xxh3_64(logical.as_bytes()));
    }

    #[test]
    fn table_rejects_distinct_string_collision() {
        // Force a collision by hashing with a constant function.
        let err = id_hash_table(|_| 1u64, ["left", "right"]).unwrap_err();
        assert!(err.contains("collision"));
        assert!(err.contains("left"));
        assert!(err.contains("right"));
    }
}

