//! Embedded browser UI assets.

/// Single-page UI for create (`/`) and simulation (`/{id}`) views.
pub const PAGE_HTML: &str = include_str!("../web/index.html");

#[cfg(test)]
mod tests {
    use super::PAGE_HTML;

    #[test]
    fn page_lists_and_invokes_actions() {
        assert!(PAGE_HTML.contains("id=\"action-panel\""));
        assert!(PAGE_HTML.contains("id=\"actionview\""));
        assert!(PAGE_HTML.contains("/actions"));
        assert!(PAGE_HTML.contains("action-run"));
        assert!(PAGE_HTML.contains("実行"));
    }
}
