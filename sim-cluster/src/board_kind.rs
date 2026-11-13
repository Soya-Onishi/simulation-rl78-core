//! Topology `BoardSpec.kind` strings and how the cluster interprets them.

/// Default RL78 board binary (`rl78-minimal-board`).
pub const KIND_RL78: &str = "rl78";
/// Default plant binary (`minimal-plant`).
pub const KIND_PLANT: &str = "plant";
/// Piping plant for the water-heater simulation.
pub const KIND_PIPING_PLANT: &str = "piping-plant";
/// Boiler (water heater) plant for the water-heater simulation.
pub const KIND_BOILER_PLANT: &str = "boiler-plant";
/// RL78 board hosting the water-heater firmware (`boiler_board`).
pub const KIND_RL78BOILER: &str = "rl78boiler";

/// True when the node runs an RL78 guest (GDB / inspect apply).
#[must_use]
pub fn is_rl78_kind(kind: &str) -> bool {
    matches!(kind, KIND_RL78 | KIND_RL78BOILER)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rl78_kinds_include_boiler_board() {
        assert!(is_rl78_kind(KIND_RL78));
        assert!(is_rl78_kind(KIND_RL78BOILER));
        assert!(!is_rl78_kind(KIND_PLANT));
        assert!(!is_rl78_kind(KIND_PIPING_PLANT));
        assert!(!is_rl78_kind(KIND_BOILER_PLANT));
    }
}
