//! Static UI helpers for `simulation-frontend`.
//!
//! The React build under `sim-cluster/out/web` is served from disk via
//! `frontend.yaml` `web_root`. This module keeps lightweight presence checks
//! for the expected built assets.

/// Relative path (from the `sim-cluster` crate) of the Vite output directory.
pub const WEB_OUT_DIR: &str = "out/web";

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    #[test]
    fn web_out_dir_name_is_stable() {
        assert_eq!(super::WEB_OUT_DIR, "out/web");
        let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        assert!(crate_dir.join(super::WEB_OUT_DIR).ends_with("out/web"));
    }
}
