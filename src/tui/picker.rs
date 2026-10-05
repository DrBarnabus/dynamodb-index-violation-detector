//! Type-to-filter list shared by the profile picker and the setup screen's
//! table field.
//!
//! Matching is a case-insensitive subsequence test, ranked so contiguous runs
//! and matches near the start of a candidate sort first.

use ratatui::text::Line;

/// A candidate list narrowed by a typed query, with one highlighted match.
#[derive(Debug, Clone, Default)]
pub struct FuzzyList {
    items: Vec<String>,
    query: String,
    matches: Vec<usize>,
    selected: usize,
}

impl FuzzyList {
    pub fn new(items: Vec<String>, query: &str) -> Self {
        let mut list = Self {
            items,
            query: query.to_string(),
            ..Self::default()
        };
        list.refilter();
        list
    }

    /// Replace the candidates, keeping the current query.
    pub fn set_items(&mut self, items: Vec<String>) {
        self.items = items;
        self.refilter();
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    /// Replace the query outright, e.g. to seed it from config.
    pub fn set_query(&mut self, query: &str) {
        self.query = query.to_string();
        self.refilter();
    }

    pub fn push(&mut self, c: char) {
        self.query.push(c);
        self.refilter();
    }

    pub fn backspace(&mut self) {
        self.query.pop();
        self.refilter();
    }

    pub fn select_next(&mut self) {
        if !self.matches.is_empty() {
            self.selected = (self.selected + 1) % self.matches.len();
        }
    }

    pub fn select_prev(&mut self) {
        if !self.matches.is_empty() {
            self.selected = (self.selected + self.matches.len() - 1) % self.matches.len();
        }
    }

    pub fn item(&self, index: usize) -> &str {
        &self.items[index]
    }

    pub fn has_items(&self) -> bool {
        !self.items.is_empty()
    }

    pub fn has_matches(&self) -> bool {
        !self.matches.is_empty()
    }

    /// Index of the highlighted candidate, if any match.
    pub fn selected_index(&self) -> Option<usize> {
        self.matches.get(self.selected).copied()
    }

    pub fn selected(&self) -> Option<&str> {
        self.selected_index().map(|i| self.item(i))
    }

    /// One line per match, at most `limit`, scrolled to keep the highlight in
    /// view. `label` renders a candidate by its index.
    pub fn lines(&self, limit: usize, label: impl Fn(usize) -> String) -> Vec<Line<'static>> {
        let start = (self.selected + 1).saturating_sub(limit);
        self.matches
            .iter()
            .enumerate()
            .skip(start)
            .take(limit)
            .map(|(position, &index)| {
                super::focusable_line(label(index), position == self.selected)
            })
            .collect()
    }

    fn refilter(&mut self) {
        let mut scored: Vec<(usize, usize)> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| score(&self.query, item).map(|s| (s, index)))
            .collect();
        scored.sort_by_key(|&(score, index)| (score, index));
        self.matches = scored.into_iter().map(|(_, index)| index).collect();
        self.selected = 0;
    }
}

/// Lower is better; `None` when `query` is not a subsequence of `candidate`.
/// The score sums the gaps between matched characters plus the offset of the
/// first match.
fn score(query: &str, candidate: &str) -> Option<usize> {
    let candidate: Vec<char> = candidate.chars().flat_map(char::to_lowercase).collect();
    let mut position = 0;
    let mut total = 0;
    let mut previous: Option<usize> = None;

    for q in query.chars().flat_map(char::to_lowercase) {
        let offset = candidate[position..].iter().position(|&c| c == q)?;
        let found = position + offset;
        total += match previous {
            Some(prev) => found - prev - 1,
            None => found,
        };
        previous = Some(found);
        position = found + 1;
    }

    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(items: &[&str]) -> FuzzyList {
        FuzzyList::new(items.iter().map(|s| s.to_string()).collect(), "")
    }

    #[test]
    fn score_rejects_non_subsequences() {
        assert_eq!(score("xyz", "users"), None);
        assert_eq!(score("sru", "users"), None);
    }

    #[test]
    fn score_prefers_contiguous_early_matches() {
        assert_eq!(score("", "anything"), Some(0));
        assert_eq!(score("use", "users"), Some(0));
        assert!(score("ord", "orders").unwrap() < score("ord", "prod-orders").unwrap());
        assert!(score("us", "users").unwrap() < score("us", "u_x_s").unwrap());
    }

    #[test]
    fn score_is_case_insensitive() {
        assert_eq!(score("USE", "users"), Some(0));
        assert_eq!(score("use", "USERS"), Some(0));
    }

    #[test]
    fn empty_query_matches_everything_in_order() {
        let list = list(&["b", "a", "c"]);
        assert_eq!(list.matches, &[0, 1, 2]);
        assert_eq!(list.selected(), Some("b"));
    }

    #[test]
    fn typing_narrows_and_ranks_matches() {
        let mut list = list(&["prod-orders", "users", "orders"]);
        list.push('o');
        list.push('r');
        list.push('d');
        assert_eq!(list.matches, &[2, 0]);
        assert_eq!(list.selected(), Some("orders"));

        list.backspace();
        list.backspace();
        list.backspace();
        assert_eq!(list.matches.len(), 3);
    }

    #[test]
    fn selection_wraps_and_resets_on_query_change() {
        let mut list = list(&["a1", "a2", "a3"]);
        list.select_prev();
        assert_eq!(list.selected(), Some("a3"));
        list.select_next();
        assert_eq!(list.selected(), Some("a1"));

        list.select_next();
        list.push('a');
        assert_eq!(list.selected(), Some("a1"));
    }

    #[test]
    fn no_match_leaves_nothing_selected() {
        let mut list = list(&["users"]);
        list.set_query("zzz");
        assert_eq!(list.selected(), None);
        list.select_next();
        assert_eq!(list.selected(), None);
    }

    #[test]
    fn set_items_keeps_the_query() {
        let mut list = list(&[]);
        list.set_query("ord");
        list.set_items(vec!["users".to_string(), "orders".to_string()]);
        assert_eq!(list.selected(), Some("orders"));
    }

    #[test]
    fn lines_scroll_to_keep_the_selection_visible() {
        let mut list = list(&["a", "b", "c", "d"]);
        list.select_next();
        list.select_next();
        list.select_next();
        let lines = list.lines(2, |i| ["a", "b", "c", "d"][i].to_string());
        let text: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        assert_eq!(text, vec!["c", "d"]);
    }
}
