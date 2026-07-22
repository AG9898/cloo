//! Server-owned copy-mode state.
//!
//! Scrollback belongs to the daemon, so selection and search cannot be client
//! cache state. This module intentionally models only positions and text
//! operations: the server supplies a current scrollback snapshot and owns the
//! emulator viewport, while a later client surface can render the positions
//! without ever altering the authoritative grid.

use std::fmt;

use regex::Regex;

/// A cell position in a pane's scrollback, counted from the oldest retained
/// line. Columns are terminal columns and `line` is not viewport-relative.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CopyPoint {
    /// Zero-based line from the oldest retained scrollback line.
    pub line: usize,
    /// Zero-based terminal column.
    pub column: u16,
}

impl CopyPoint {
    /// Creates one scrollback position.
    #[must_use]
    pub const fn new(line: usize, column: u16) -> Self {
        Self { line, column }
    }
}

/// A linear selection, retaining both the fixed anchor and moving head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopySelection {
    /// Position at which selection began.
    pub anchor: CopyPoint,
    /// Current cursor position.
    pub head: CopyPoint,
}

impl CopySelection {
    /// The first selected cell in scrollback order.
    #[must_use]
    pub fn start(self) -> CopyPoint {
        self.anchor.min(self.head)
    }

    /// The last selected cell in scrollback order.
    #[must_use]
    pub fn end(self) -> CopyPoint {
        self.anchor.max(self.head)
    }
}

/// One regex match, with an exclusive end position suitable for highlighting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchMatch {
    /// First matched cell.
    pub start: CopyPoint,
    /// One cell after the match's final cell.
    pub end: CopyPoint,
}

/// The direction in which a search walks its result set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchDirection {
    /// Toward newer scrollback lines.
    Forward,
    /// Toward older scrollback lines.
    Backward,
}

/// One copy-mode navigation command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyMotion {
    /// Vim `h`.
    Left,
    /// Vim `j`.
    Down,
    /// Vim `k`.
    Up,
    /// Vim `l`.
    Right,
    /// Vim `w`.
    WordForward,
    /// Vim `b`.
    WordBackward,
    /// Vim `0`.
    LineStart,
    /// Vim `$`.
    LineEnd,
    /// Vim `g`.
    FirstLine,
    /// Vim `G`.
    LastLine,
}

/// A validated regex search and its current result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchState {
    query: String,
    matches: Vec<SearchMatch>,
    current: Option<usize>,
}

impl SearchState {
    /// The regex text the user supplied.
    #[must_use]
    pub fn query(&self) -> &str {
        &self.query
    }

    /// Every non-empty match in scrollback order.
    #[must_use]
    pub fn matches(&self) -> &[SearchMatch] {
        &self.matches
    }

    /// The match at the copy cursor, when the query matched anything.
    #[must_use]
    pub fn current(&self) -> Option<SearchMatch> {
        self.current
            .and_then(|index| self.matches.get(index).copied())
    }
}

/// A regex search that could not be compiled.
#[derive(Debug)]
pub struct SearchError(regex::Error);

impl fmt::Display for SearchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid copy-mode regex: {}", self.0)
    }
}

impl std::error::Error for SearchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

/// Copy-mode state for one pane.
///
/// It has no terminal or socket dependency: callers pass the latest retained
/// lines and terminal width to every operation. That keeps search errors and
/// motions pure, while the session actor decides which pane and viewport the
/// state belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyMode {
    cursor: CopyPoint,
    anchor: Option<CopyPoint>,
    search: Option<SearchState>,
}

impl CopyMode {
    /// Begins at the newest visible line, column zero.
    #[must_use]
    pub fn new(lines: &[String], columns: u16) -> Self {
        let last_line = lines.len().saturating_sub(1);
        let cursor = clamp_point(CopyPoint::new(last_line, 0), lines, columns);
        Self {
            cursor,
            anchor: None,
            search: None,
        }
    }

    /// Current copy cursor.
    #[must_use]
    pub fn cursor(&self) -> CopyPoint {
        self.cursor
    }

    /// Current linear selection, if visual selection has begun.
    #[must_use]
    pub fn selection(&self) -> Option<CopySelection> {
        self.anchor.map(|anchor| CopySelection {
            anchor,
            head: self.cursor,
        })
    }

    /// The active regex search, if one has been issued.
    #[must_use]
    pub fn search_state(&self) -> Option<&SearchState> {
        self.search.as_ref()
    }

    /// Starts visual selection at the current cursor. Repeating it keeps the
    /// original anchor; use [`clear_selection`](Self::clear_selection) before
    /// starting a new selection.
    pub fn begin_selection(&mut self) {
        if self.anchor.is_none() {
            self.anchor = Some(self.cursor);
        }
    }

    /// Leaves visual selection without moving the cursor.
    pub fn clear_selection(&mut self) {
        self.anchor = None;
    }

    /// Moves the cursor using a vim-like motion and clamps it to retained text.
    ///
    /// The result says whether the position actually changed. Empty scrollback
    /// is a valid no-op state, rather than a source of a zero-line underflow.
    pub fn move_cursor(&mut self, motion: CopyMotion, lines: &[String], columns: u16) -> bool {
        let before = self.cursor;
        self.cursor = clamp_point(self.cursor, lines, columns);
        if lines.is_empty() || columns == 0 {
            return false;
        }

        let max_column = columns - 1;
        match motion {
            CopyMotion::Left => self.cursor.column = self.cursor.column.saturating_sub(1),
            CopyMotion::Right => {
                self.cursor.column = self.cursor.column.saturating_add(1).min(max_column);
            }
            CopyMotion::Up => {
                self.cursor.line = self.cursor.line.saturating_sub(1);
                self.cursor = clamp_point(self.cursor, lines, columns);
            }
            CopyMotion::Down => {
                self.cursor.line = self.cursor.line.saturating_add(1).min(lines.len() - 1);
                self.cursor = clamp_point(self.cursor, lines, columns);
            }
            CopyMotion::LineStart => self.cursor.column = 0,
            CopyMotion::LineEnd => {
                self.cursor.column = line_end(&lines[self.cursor.line], max_column)
            }
            CopyMotion::FirstLine => {
                self.cursor.line = 0;
                self.cursor = clamp_point(self.cursor, lines, columns);
            }
            CopyMotion::LastLine => {
                self.cursor.line = lines.len() - 1;
                self.cursor = clamp_point(self.cursor, lines, columns);
            }
            CopyMotion::WordForward => move_word_forward(&mut self.cursor, lines, columns),
            CopyMotion::WordBackward => move_word_backward(&mut self.cursor, lines, columns),
        }
        self.cursor != before
    }

    /// Compiles `query`, finds every non-empty match in the supplied retained
    /// text, and moves to the first result in `direction`, wrapping once.
    ///
    /// Searching line by line is intentional: terminal soft wraps and hard
    /// line boundaries are rendering details, so a regex cannot silently match
    /// text from two separate terminal lines.
    pub fn search(
        &mut self,
        query: impl Into<String>,
        direction: SearchDirection,
        lines: &[String],
        columns: u16,
    ) -> Result<bool, SearchError> {
        let query = query.into();
        let regex = Regex::new(&query).map_err(SearchError)?;
        let matches = find_matches(&regex, lines, columns);
        let current = select_match(&matches, self.cursor, direction);
        let found = current.is_some();
        if let Some(index) = current {
            self.cursor = matches[index].start;
        }
        self.search = Some(SearchState {
            query,
            matches,
            current,
        });
        Ok(found)
    }

    /// Re-runs the current valid query after terminal output changed retained
    /// scrollback. A regex was compiled before it was stored, so this can only
    /// be a no-op on an impossible parser failure rather than a new user error.
    pub fn refresh_search(&mut self, lines: &[String], columns: u16) {
        let Some(query) = self.search.as_ref().map(|search| search.query.clone()) else {
            return;
        };
        let Ok(regex) = Regex::new(&query) else {
            return;
        };
        let matches = find_matches(&regex, lines, columns);
        let current = select_match(&matches, self.cursor, SearchDirection::Forward);
        self.search = Some(SearchState {
            query,
            matches,
            current,
        });
        self.cursor = clamp_point(self.cursor, lines, columns);
        if let Some(anchor) = self.anchor {
            self.anchor = Some(clamp_point(anchor, lines, columns));
        }
    }

    /// Moves to the next or previous result of the active search, wrapping at
    /// the end. Returns false if there is no active query or it matched nothing.
    pub fn search_next(&mut self, direction: SearchDirection) -> bool {
        let Some(search) = self.search.as_mut() else {
            return false;
        };
        let Some(current) = search.current else {
            return false;
        };
        if search.matches.is_empty() {
            return false;
        }
        let next = match direction {
            SearchDirection::Forward => (current + 1) % search.matches.len(),
            SearchDirection::Backward => {
                if current == 0 {
                    search.matches.len() - 1
                } else {
                    current - 1
                }
            }
        };
        let before = self.cursor;
        self.cursor = search.matches[next].start;
        search.current = Some(next);
        self.cursor != before
    }

    /// Extracts the current linear selection from retained text, preserving
    /// line breaks. It is a pure helper for the later, explicitly permitted
    /// clipboard effect; it never mutates a terminal cell.
    #[must_use]
    pub fn selected_text(&self, lines: &[String], columns: u16) -> Option<String> {
        let selection = self.selection()?;
        if lines.is_empty() || columns == 0 {
            return Some(String::new());
        }
        let start = clamp_point(selection.start(), lines, columns);
        let end = clamp_point(selection.end(), lines, columns);
        let mut selected = Vec::new();
        for (line_index, line) in lines
            .iter()
            .enumerate()
            .skip(start.line)
            .take(end.line.saturating_sub(start.line).saturating_add(1))
        {
            let chars: Vec<char> = line.chars().collect();
            let first = if line_index == start.line {
                usize::from(start.column)
            } else {
                0
            };
            let last = if line_index == end.line {
                usize::from(end.column)
            } else {
                usize::from(columns - 1)
            };
            selected.push(
                chars
                    .get(first..=last.min(chars.len().saturating_sub(1)))
                    .map_or_else(String::new, |cells| cells.iter().collect()),
            );
        }
        Some(selected.join("\n"))
    }
}

fn find_matches(regex: &Regex, lines: &[String], columns: u16) -> Vec<SearchMatch> {
    lines
        .iter()
        .enumerate()
        .flat_map(|(line, text)| {
            regex.find_iter(text).filter_map(move |matched| {
                if matched.start() == matched.end() {
                    return None;
                }
                let start = text[..matched.start()].chars().count();
                let end = text[..matched.end()].chars().count();
                let start = u16::try_from(start).ok()?;
                let end = u16::try_from(end).ok()?;
                if start >= columns || end > columns {
                    return None;
                }
                Some(SearchMatch {
                    start: CopyPoint::new(line, start),
                    end: CopyPoint::new(line, end),
                })
            })
        })
        .collect()
}

fn select_match(
    matches: &[SearchMatch],
    cursor: CopyPoint,
    direction: SearchDirection,
) -> Option<usize> {
    match direction {
        SearchDirection::Forward => matches
            .iter()
            .position(|matched| matched.start >= cursor)
            .or_else(|| (!matches.is_empty()).then_some(0)),
        SearchDirection::Backward => matches
            .iter()
            .rposition(|matched| matched.start <= cursor)
            .or_else(|| (!matches.is_empty()).then_some(matches.len() - 1)),
    }
}

fn clamp_point(point: CopyPoint, lines: &[String], columns: u16) -> CopyPoint {
    if lines.is_empty() || columns == 0 {
        return CopyPoint::new(0, 0);
    }
    let line = point.line.min(lines.len() - 1);
    CopyPoint::new(line, point.column.min(line_end(&lines[line], columns - 1)))
}

fn line_end(line: &str, max_column: u16) -> u16 {
    let width = line.chars().count().saturating_sub(1);
    u16::try_from(width).unwrap_or(max_column).min(max_column)
}

fn move_word_forward(cursor: &mut CopyPoint, lines: &[String], columns: u16) {
    let mut line = cursor.line;
    let mut column = usize::from(cursor.column).saturating_add(1);
    let mut leaving_current_word = true;
    while line < lines.len() {
        let chars: Vec<char> = lines[line].chars().collect();
        if leaving_current_word {
            while column < chars.len() && is_word(chars[column]) {
                column += 1;
            }
            leaving_current_word = false;
        }
        while column < chars.len() && !is_word(chars[column]) {
            column += 1;
        }
        if column < chars.len() {
            cursor.line = line;
            cursor.column = u16::try_from(column)
                .unwrap_or(columns - 1)
                .min(columns - 1);
            return;
        }
        line += 1;
        column = 0;
    }
    *cursor = clamp_point(*cursor, lines, columns);
}

fn move_word_backward(cursor: &mut CopyPoint, lines: &[String], columns: u16) {
    let mut line = cursor.line;
    let mut column = usize::from(cursor.column);
    loop {
        let chars: Vec<char> = lines[line].chars().collect();
        column = column.min(chars.len());
        column = column.saturating_sub(1);
        while column > 0 && !is_word(chars[column]) {
            column -= 1;
        }
        if !chars.is_empty() && is_word(chars[column]) {
            while column > 0 && is_word(chars[column - 1]) {
                column -= 1;
            }
            cursor.line = line;
            cursor.column = u16::try_from(column)
                .unwrap_or(columns - 1)
                .min(columns - 1);
            return;
        }
        if line == 0 {
            *cursor = clamp_point(*cursor, lines, columns);
            return;
        }
        line -= 1;
        column = lines[line].chars().count();
    }
}

fn is_word(ch: char) -> bool {
    ch == '_' || ch.is_alphanumeric()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines() -> Vec<String> {
        vec!["alpha beta".into(), "gamma_delta".into(), "omega".into()]
    }

    #[test]
    fn vim_motions_clamp_to_retained_scrollback() {
        let lines = lines();
        let mut copy = CopyMode::new(&lines, 12);
        assert_eq!(copy.cursor(), CopyPoint::new(2, 0));
        assert!(copy.move_cursor(CopyMotion::FirstLine, &lines, 12));
        assert!(copy.move_cursor(CopyMotion::LineEnd, &lines, 12));
        assert_eq!(copy.cursor(), CopyPoint::new(0, 9));
        assert!(copy.move_cursor(CopyMotion::WordBackward, &lines, 12));
        assert_eq!(copy.cursor(), CopyPoint::new(0, 6));
        assert!(copy.move_cursor(CopyMotion::WordForward, &lines, 12));
        assert_eq!(copy.cursor(), CopyPoint::new(1, 0));
        assert!(copy.move_cursor(CopyMotion::LastLine, &lines, 12));
        assert_eq!(copy.cursor(), CopyPoint::new(2, 0));
        assert!(!copy.move_cursor(CopyMotion::Down, &lines, 12));
    }

    #[test]
    fn selection_keeps_its_anchor_through_motion_and_extracts_text() {
        let lines = lines();
        let mut copy = CopyMode::new(&lines, 12);
        let _ = copy.move_cursor(CopyMotion::FirstLine, &lines, 12);
        let _ = copy.move_cursor(CopyMotion::Right, &lines, 12);
        copy.begin_selection();
        let _ = copy.move_cursor(CopyMotion::Down, &lines, 12);
        let _ = copy.move_cursor(CopyMotion::LineEnd, &lines, 12);
        assert_eq!(
            copy.selection(),
            Some(CopySelection {
                anchor: CopyPoint::new(0, 1),
                head: CopyPoint::new(1, 10),
            })
        );
        assert_eq!(
            copy.selected_text(&lines, 12).as_deref(),
            Some("lpha beta\ngamma_delta")
        );
        copy.clear_selection();
        assert_eq!(copy.selection(), None);
    }

    #[test]
    fn regex_search_records_matches_wraps_and_reports_errors_cleanly() {
        let lines = lines();
        let mut copy = CopyMode::new(&lines, 12);
        let _ = copy.move_cursor(CopyMotion::FirstLine, &lines, 12);
        assert!(
            copy.search("a", SearchDirection::Forward, &lines, 12)
                .expect("valid regex")
        );
        assert_eq!(copy.cursor(), CopyPoint::new(0, 0));
        assert_eq!(
            copy.search_state().map(|search| search.matches().len()),
            Some(7)
        );
        assert!(copy.search_next(SearchDirection::Backward));
        assert_eq!(copy.cursor(), CopyPoint::new(2, 4));

        let error = copy
            .search("(", SearchDirection::Forward, &lines, 12)
            .expect_err("invalid regex must not panic");
        assert!(error.to_string().contains("invalid copy-mode regex"));
        assert_eq!(copy.search_state().map(SearchState::query), Some("a"));
    }

    #[test]
    fn empty_text_and_zero_width_are_safe_no_ops() {
        let mut copy = CopyMode::new(&[], 0);
        assert!(!copy.move_cursor(CopyMotion::Right, &[], 0));
        assert_eq!(copy.selected_text(&[], 0), None);
        assert!(
            !copy
                .search(".", SearchDirection::Forward, &[], 0)
                .expect("valid regex")
        );
        assert_eq!(
            copy.search_state().map(|search| search.matches()),
            Some(&[][..])
        );
    }
}
