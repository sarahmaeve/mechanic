//! Search and selection for the native command palette, independent of AppKit.

use mechanic_core::search::normalize_search_text;

pub const MAX_QUERY_CHARS: usize = 256;
const MAX_ENTRY_CHARS: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteEntry {
    pub id: String,
    pub label: String,
    pub shortcut: Option<String>,
}

#[derive(Default)]
pub struct PaletteModel {
    entries: Vec<PaletteEntry>,
    searchable: Vec<Option<String>>,
    query: String,
    filtered: Vec<usize>,
    selected: Option<usize>,
    query_too_long: bool,
}

impl PaletteModel {
    pub fn set_entries(&mut self, entries: Vec<PaletteEntry>) {
        let selected_id = self.selected().map(|entry| entry.id.clone());
        self.searchable = entries
            .iter()
            .map(|entry| {
                normalize_search_text(
                    &format!("{} {}", entry.label, entry.shortcut.as_deref().unwrap_or_default()),
                    MAX_ENTRY_CHARS,
                )
            })
            .collect();
        self.entries = entries;
        self.filter();
        if let Some(id) = selected_id
            && let Some(index) =
                self.filtered.iter().position(|index| self.entries[*index].id == id)
        {
            self.selected = Some(index);
        }
    }

    pub fn set_query(&mut self, query: &str) {
        // Inspect only the bounded prefix. Long pasted input must not make the
        // command search allocate or scan proportionally to that input.
        let normalized = normalize_search_text(query, MAX_QUERY_CHARS);
        self.query_too_long = normalized.is_none();
        self.query = normalized.unwrap_or_default();
        self.filter();
    }

    fn filter(&mut self) {
        self.filtered.clear();
        self.selected = None;
        if self.query_too_long {
            return;
        }
        let tokens: Vec<_> = self.query.split_whitespace().collect();
        self.filtered.extend(self.searchable.iter().enumerate().filter_map(|(index, text)| {
            (tokens.is_empty()
                || text
                    .as_ref()
                    .is_some_and(|text| tokens.iter().all(|token| text.contains(token))))
            .then_some(index)
        }));
        self.selected = (!self.filtered.is_empty()).then_some(0);
    }

    pub fn move_selection(&mut self, backwards: bool) {
        let count = self.filtered.len();
        if count == 0 {
            self.selected = None;
            return;
        }
        self.selected = Some(match self.selected {
            Some(index) if backwards => (index + count - 1) % count,
            Some(index) => (index + 1) % count,
            None if backwards => count - 1,
            None => 0,
        });
    }

    pub fn select(&mut self, index: usize) {
        if index < self.filtered.len() {
            self.selected = Some(index);
        }
    }

    pub fn selected_index(&self) -> Option<usize> {
        self.selected
    }

    pub fn selected(&self) -> Option<&PaletteEntry> {
        self.selected.and_then(|index| self.entry(index))
    }

    pub fn entry(&self, index: usize) -> Option<&PaletteEntry> {
        self.filtered.get(index).map(|index| &self.entries[*index])
    }

    pub fn count(&self) -> usize {
        self.filtered.len()
    }

    pub fn query_too_long(&self) -> bool {
        self.query_too_long
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, label: &str) -> PaletteEntry {
        PaletteEntry { id: id.into(), label: label.into(), shortcut: None }
    }

    #[test]
    fn filter_matches_case_insensitive_tokens_in_any_order_and_keeps_ids() {
        let mut model = PaletteModel::default();
        model.set_entries(vec![
            entry("split-right", "Split Pane Right"),
            entry("load-dev", "Open Loadout · Développement"),
            entry("split-down", "Split Pane Down"),
        ]);
        model.set_query(" right SPLIT ");
        assert_eq!(model.count(), 1);
        assert_eq!(model.selected().unwrap().id, "split-right");
        model.set_query("dével");
        assert_eq!(model.selected().unwrap().id, "load-dev");
        model.set_query("never matches");
        model.move_selection(true);
        assert_eq!(model.selected(), None);
    }

    #[test]
    fn keyboard_wraps_and_entry_changes_preserve_selection_by_identity() {
        let mut model = PaletteModel::default();
        model.set_entries(vec![entry("a", "Alpha"), entry("b", "Beta")]);
        model.move_selection(true);
        assert_eq!(model.selected().unwrap().id, "b");
        model.move_selection(false);
        assert_eq!(model.selected().unwrap().id, "a");
        model.set_entries(vec![entry("b", "Beta"), entry("a", "Alpha"), entry("c", "Gamma")]);
        assert_eq!(model.selected_index(), Some(1));
        model.select(999);
        assert_eq!(model.selected_index(), Some(1));
        model.set_entries(vec![entry("c", "Gamma")]);
        assert_eq!(model.selected().unwrap().id, "c");
        model.set_entries(Vec::new());
        assert_eq!(model.selected_index(), None);
    }

    #[test]
    fn oversized_query_has_no_partial_match_and_recovers_after_editing() {
        let mut model = PaletteModel::default();
        model.set_entries(vec![entry("a", "Alpha")]);
        model.set_query(&"界".repeat(MAX_QUERY_CHARS + 1));
        assert!(model.query_too_long());
        assert_eq!(model.count(), 0);
        model.set_query("al");
        assert!(!model.query_too_long());
        assert_eq!(model.selected().unwrap().id, "a");
    }

    #[test]
    fn unicode_filter_folds_german_spelling_and_canonical_accents() {
        let mut model = PaletteModel::default();
        model.set_entries(vec![
            entry("street", "Open loadout · Straße"),
            entry("muller", "Open loadout · Mu\u{308}ller"),
            entry("accent", "Open loadout · Café"),
        ]);
        for (query, id) in [("STRASSE", "street"), ("MÜLLER", "muller"), ("cafe\u{301}", "accent")]
        {
            model.set_query(query);
            assert_eq!(model.count(), 1, "query {query:?}");
            assert_eq!(model.selected().unwrap().id, id);
        }
        model.set_query(&"ß".repeat(MAX_QUERY_CHARS / 2 + 1));
        assert!(model.query_too_long(), "fold expansion must respect the search limit");
        assert_eq!(model.count(), 0);
    }
}
