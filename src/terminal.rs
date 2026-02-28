// vt100 wrapper, scrollback management per process.

pub struct Terminal {
    parser: vt100::Parser,
    scrollback_len: usize,
    search_dirty: bool,
    cached_all_query: String,
    cached_all_matches: Vec<(usize, usize, usize)>,
    cached_all_total_rows: usize,
    cached_visible_query: String,
    cached_visible_scrollback: usize,
    cached_visible_matches: Vec<(u16, usize, usize)>,
}

impl Terminal {
    pub fn new(rows: u16, cols: u16, scrollback_len: usize) -> Self {
        Self {
            parser: vt100::Parser::new(rows, cols, scrollback_len),
            scrollback_len,
            search_dirty: true,
            cached_all_query: String::new(),
            cached_all_matches: Vec::new(),
            cached_all_total_rows: 0,
            cached_visible_query: String::new(),
            cached_visible_scrollback: usize::MAX,
            cached_visible_matches: Vec::new(),
        }
    }

    pub fn process(&mut self, data: &[u8]) {
        // When scrolled back, compensate for new lines entering the scrollback
        // buffer so the viewport stays pinned to the same content.
        let sb = self.parser.screen().scrollback();
        let max_before = if sb > 0 {
            self.parser.screen_mut().set_scrollback(self.scrollback_len);
            let max = self.parser.screen().scrollback();
            self.parser.screen_mut().set_scrollback(sb);
            Some(max)
        } else {
            None
        };

        self.parser.process(data);
        self.search_dirty = true;

        if let Some(max_before) = max_before {
            self.parser.screen_mut().set_scrollback(self.scrollback_len);
            let max_after = self.parser.screen().scrollback();
            self.parser
                .screen_mut()
                .set_scrollback(sb + max_after.saturating_sub(max_before));
        }
    }

    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.parser.screen_mut().set_size(rows, cols);
        self.search_dirty = true;
    }

    pub fn scroll_up(&mut self, lines: usize) {
        if self.parser.screen().alternate_screen() {
            return;
        }
        let current = self.parser.screen().scrollback();
        self.parser
            .screen_mut()
            .set_scrollback(current.saturating_add(lines));
    }

    pub fn scroll_down(&mut self, lines: usize) {
        if self.parser.screen().alternate_screen() {
            return;
        }
        let current = self.parser.screen().scrollback();
        self.parser
            .screen_mut()
            .set_scrollback(current.saturating_sub(lines));
    }

    pub fn scroll_to_bottom(&mut self) {
        self.parser.screen_mut().set_scrollback(0);
    }

    pub fn scroll_to_top(&mut self) {
        if self.parser.screen().alternate_screen() {
            return;
        }
        self.parser.screen_mut().set_scrollback(self.scrollback_len);
    }

    pub fn is_scrolled_back(&self) -> bool {
        self.parser.screen().scrollback() > 0
    }

    pub fn scrollback(&self) -> usize {
        self.parser.screen().scrollback()
    }

    pub fn size(&self) -> (u16, u16) {
        self.parser.screen().size()
    }

    pub fn is_alternate_screen(&self) -> bool {
        self.parser.screen().alternate_screen()
    }

    /// Extract text content from a single visible row, skipping wide continuation
    /// cells. Returns the clean text and a mapping from char index to display column.
    fn row_text_with_columns(&self, row: u16) -> (String, Vec<u16>) {
        let (_, cols) = self.parser.screen().size();
        let mut text = String::with_capacity(cols as usize);
        let mut char_to_col: Vec<u16> = Vec::with_capacity(cols as usize);
        for col in 0..cols {
            let cell = self.parser.screen().cell(row, col);
            if cell.is_some_and(|c| c.is_wide_continuation()) {
                continue;
            }
            char_to_col.push(col);
            text.push(cell.map_or(' ', |c| c.contents().chars().next().unwrap_or(' ')));
        }
        (text, char_to_col)
    }

    /// Find case-insensitive matches of `query` on currently visible screen rows.
    /// Returns (screen_row, col, len) tuples.
    pub fn find_visible_matches(&mut self, query: &str) -> &[(u16, usize, usize)] {
        if query.is_empty() {
            return &[];
        }
        let scrollback = self.parser.screen().scrollback();
        if !self.search_dirty
            && self.cached_visible_query == query
            && self.cached_visible_scrollback == scrollback
        {
            return &self.cached_visible_matches;
        }
        let (rows, cols) = self.parser.screen().size();
        let query_lower = query.to_lowercase();
        let query_chars = query_lower.chars().count();
        let mut matches = Vec::new();
        for row in 0..rows {
            let (text, char_to_col) = self.row_text_with_columns(row);
            let text_lower = text.to_lowercase();
            for (byte_start, _) in text_lower.match_indices(&query_lower) {
                let (start_col, len) =
                    match_display_span(&text_lower, byte_start, query_chars, &char_to_col, cols);
                matches.push((row, start_col, len));
            }
        }
        self.cached_visible_query = query.to_string();
        self.cached_visible_scrollback = scrollback;
        self.cached_visible_matches = matches;
        &self.cached_visible_matches
    }

    /// Find all case-insensitive matches of `query` across all scrollback + visible rows.
    /// Returns (matches, total_rows) where matches are (absolute_row, col, len) tuples.
    pub fn find_all_matches(&mut self, query: &str) -> (&[(usize, usize, usize)], usize) {
        if query.is_empty() {
            return (&[], 0);
        }
        if !self.search_dirty && self.cached_all_query == query {
            return (&self.cached_all_matches, self.cached_all_total_rows);
        }
        let saved_scrollback = self.parser.screen().scrollback();
        let (rows, _) = self.parser.screen().size();
        let rows = rows as usize;

        // Scroll to the very top to read all content.
        self.parser.screen_mut().set_scrollback(self.scrollback_len);
        let actual_scrollback = self.parser.screen().scrollback();
        let total_rows = actual_scrollback + rows;

        let (_, cols) = self.parser.screen().size();
        let query_lower = query.to_lowercase();
        let query_chars = query_lower.chars().count();
        let mut matches = Vec::new();

        for abs_row in 0..total_rows {
            // We need to set scrollback so that `abs_row` is visible on screen.
            // When scrollback = S, the screen shows rows from (total - rows - S) to (total - S - 1)
            // relative to the full buffer. We want abs_row to be visible.
            let scrollback_for_row = actual_scrollback.saturating_sub(abs_row);
            self.parser.screen_mut().set_scrollback(scrollback_for_row);

            let screen_row =
                abs_row.saturating_sub(actual_scrollback.saturating_sub(scrollback_for_row));
            let (text, char_to_col) = self.row_text_with_columns(screen_row as u16);
            let text_lower = text.to_lowercase();

            for (byte_start, _) in text_lower.match_indices(&query_lower) {
                let (start_col, len) =
                    match_display_span(&text_lower, byte_start, query_chars, &char_to_col, cols);
                matches.push((abs_row, start_col, len));
            }
        }

        self.parser.screen_mut().set_scrollback(saved_scrollback);
        self.cached_all_query = query.to_string();
        self.cached_all_matches = matches;
        self.cached_all_total_rows = total_rows;
        self.search_dirty = false;
        (&self.cached_all_matches, self.cached_all_total_rows)
    }

    /// Scroll so that the given absolute row is roughly centered in the viewport.
    pub fn scroll_to_row(&mut self, abs_row: usize) {
        if self.parser.screen().alternate_screen() {
            return;
        }
        let (rows, _) = self.parser.screen().size();
        let rows = rows as usize;

        // Figure out max scrollback.
        self.parser.screen_mut().set_scrollback(self.scrollback_len);
        let max_scrollback = self.parser.screen().scrollback();

        // abs_row 0 is the topmost row. scrollback = max_scrollback shows the top.
        // scrollback = max_scrollback - abs_row puts abs_row at the top of viewport.
        // To center, add half the viewport height. Use signed math to handle
        // matches near the bottom where abs_row > max_scrollback.
        let target = (max_scrollback as isize - abs_row as isize + (rows / 2) as isize)
            .clamp(0, max_scrollback as isize) as usize;
        self.parser.screen_mut().set_scrollback(target);
    }
}

/// Convert a byte-offset match into display column coordinates.
/// Returns (start_col, display_len).
fn match_display_span(
    text: &str,
    byte_start: usize,
    query_chars: usize,
    char_to_col: &[u16],
    cols: u16,
) -> (usize, usize) {
    let char_start = text[..byte_start].chars().count();
    let char_end = char_start + query_chars;
    let start_col = char_to_col[char_start] as usize;
    let end_col = if char_end < char_to_col.len() {
        char_to_col[char_end] as usize
    } else {
        cols as usize
    };
    (start_col, end_col - start_col)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_terminal_has_correct_size() {
        let term = Terminal::new(24, 80, 100);
        assert_eq!(term.size(), (24, 80));
    }

    #[test]
    fn process_renders_text() {
        let mut term = Terminal::new(24, 80, 100);
        term.process(b"hello world");
        assert!(term.screen().contents().contains("hello world"));
    }

    #[test]
    fn scroll_up_and_down() {
        let mut term = Terminal::new(5, 80, 100);
        // Fill scrollback by writing more lines than screen height.
        for i in 0..20 {
            term.process(format!("line {i}\r\n").as_bytes());
        }
        assert!(!term.is_scrolled_back());

        term.scroll_up(3);
        assert!(term.is_scrolled_back());
        assert_eq!(term.screen().scrollback(), 3);

        term.scroll_down(2);
        assert_eq!(term.screen().scrollback(), 1);

        term.scroll_down(10);
        assert!(!term.is_scrolled_back());
    }

    #[test]
    fn scroll_to_top_and_bottom() {
        let mut term = Terminal::new(5, 80, 100);
        for i in 0..20 {
            term.process(format!("line {i}\r\n").as_bytes());
        }

        term.scroll_to_top();
        assert!(term.is_scrolled_back());

        term.scroll_to_bottom();
        assert!(!term.is_scrolled_back());
    }

    #[test]
    fn new_output_preserves_scroll_content() {
        let mut term = Terminal::new(5, 80, 100);
        for i in 0..20 {
            term.process(format!("line {i}\r\n").as_bytes());
        }
        term.scroll_up(5);
        let contents_before = term.screen().contents();

        term.process(b"more output\r\n");
        assert!(term.is_scrolled_back());
        assert_eq!(term.screen().contents(), contents_before);
    }

    #[test]
    fn scroll_ignored_on_alternate_screen() {
        let mut term = Terminal::new(5, 80, 100);
        for i in 0..20 {
            term.process(format!("line {i}\r\n").as_bytes());
        }
        // Enter alternate screen.
        term.process(b"\x1b[?1049h");

        term.scroll_up(5);
        assert!(!term.is_scrolled_back());

        term.scroll_to_top();
        assert!(!term.is_scrolled_back());
    }

    #[test]
    fn resize_changes_size() {
        let mut term = Terminal::new(24, 80, 100);
        term.resize(40, 120);
        assert_eq!(term.size(), (40, 120));
    }

    #[test]
    fn find_visible_matches_ascii() {
        let mut term = Terminal::new(5, 20, 100);
        term.process(b"hello world\r\nfoo hello bar\r\n");
        let matches = term.find_visible_matches("hello").to_vec();
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0], (0, 0, 5)); // row 0, col 0, len 5
        assert_eq!(matches[1], (1, 4, 5)); // row 1, col 4, len 5
    }

    #[test]
    fn find_visible_matches_case_insensitive() {
        let mut term = Terminal::new(5, 20, 100);
        term.process(b"Hello HELLO hElLo\r\n");
        let matches = term.find_visible_matches("hello").to_vec();
        assert_eq!(matches.len(), 3);
    }

    #[test]
    fn find_visible_matches_empty_query() {
        let mut term = Terminal::new(5, 20, 100);
        term.process(b"hello world\r\n");
        let matches = term.find_visible_matches("");
        assert!(matches.is_empty());
    }

    #[test]
    fn find_visible_matches_multibyte_utf8() {
        let mut term = Terminal::new(5, 40, 100);
        // vt100 renders wide chars as 2 cells, but row_text produces one
        // char per cell. The key thing: byte offsets in the lowercased
        // string must be converted to char (cell) offsets.
        term.process("café match\r\n".as_bytes());
        let matches = term.find_visible_matches("match").to_vec();
        assert_eq!(matches.len(), 1);
        // 'é' takes 1 cell in row_text, so "café " is 5 chars, "match" starts at col 5
        assert_eq!(matches[0], (0, 5, 5));
    }

    #[test]
    fn find_all_matches_across_scrollback() {
        let mut term = Terminal::new(3, 20, 100);
        for i in 0..10 {
            term.process(format!("line {i} needle\r\n").as_bytes());
        }
        let (matches, total_rows) = term.find_all_matches("needle");
        let matches = matches.to_vec();
        assert_eq!(matches.len(), 10);
        assert!(total_rows >= 10);
        // Matches should be in ascending row order.
        for pair in matches.windows(2) {
            assert!(pair[0].0 < pair[1].0);
        }
    }

    #[test]
    fn find_visible_matches_wide_char_before_match() {
        let mut term = Terminal::new(5, 40, 100);
        // "占" is a wide char taking 2 display columns
        term.process("占test\r\n".as_bytes());
        let matches = term.find_visible_matches("test").to_vec();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0], (0, 2, 4));
    }

    #[test]
    fn find_visible_matches_wide_char_inside_match() {
        let mut term = Terminal::new(5, 40, 100);
        term.process("ab占cd\r\n".as_bytes());
        let matches = term.find_visible_matches("占cd").to_vec();
        assert_eq!(matches.len(), 1);
        // "占" starts at display col 2, takes 2 cols; "cd" at cols 4-5; total len 4
        assert_eq!(matches[0], (0, 2, 4));
    }

    #[test]
    fn search_cache_invalidated_by_process() {
        let mut term = Terminal::new(5, 20, 100);
        term.process(b"hello\r\n");
        let (matches1, _) = term.find_all_matches("hello");
        let count1 = matches1.len();

        // Add more content — cache should be invalidated.
        term.process(b"hello again\r\n");
        let (matches2, _) = term.find_all_matches("hello");
        let count2 = matches2.len();
        assert!(count2 > count1);
    }
}
