//! Deterministic board-id hashing for wire payloads.

use std::collections::{HashMap, HashSet};

use xxhash_rust::xxh3::xxh3_64;

/// Stable 64-bit hash of a board id (UTF-8 bytes). Same input → same hash across processes.
#[must_use]
pub fn board_id_hash(board_id: &str) -> u64 {
    xxh3_64(board_id.as_bytes())
}

/// Build reverse lookup; errors if two board ids collide.
pub fn board_hash_table<'a>(
    board_ids: impl IntoIterator<Item = &'a str>,
) -> Result<HashMap<u64, String>, String> {
    let mut seen = HashSet::new();
    let mut table = HashMap::new();
    for id in board_ids {
        let h = board_id_hash(id);
        if !seen.insert(h) {
            return Err(format!("board_id hash collision for '{id}' (hash={h:#x})"));
        }
        table.insert(h, id.to_string());
    }
    Ok(table)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable() {
        assert_eq!(board_id_hash("a"), board_id_hash("a"));
        assert_ne!(board_id_hash("a"), board_id_hash("b"));
    }

    #[test]
    fn table_rejects_duplicate_ids() {
        // Same id twice is a collision on the hash set insert of the same hash.
        let err = board_hash_table(["a", "a"]).unwrap_err();
        assert!(err.contains("collision"));
    }
}
