// Single source of truth for keybinding hints (status bar + help panel).

use crate::process::State;
use ratatui::style::{Color, Style};
use ratatui::text::Span;

const KEY_STYLE: Style = Style::new().fg(Color::White).bold();
const SEP_STYLE: Style = Style::new();

/// Render a key string into styled spans: keys bold, `/` separators default.
/// A bare `"/"` is treated as a literal key, not a separator.
pub fn styled_keys(keys: &str, spans: &mut Vec<Span<'static>>) {
    if !keys.contains('/') || keys == "/" {
        spans.push(Span::styled(keys.to_string(), KEY_STYLE));
        return;
    }
    for (i, key) in keys.split('/').enumerate() {
        if i > 0 {
            spans.push(Span::styled("/", SEP_STYLE));
        }
        spans.push(Span::styled(key.to_string(), KEY_STYLE));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Navigation,
    Actions,
    App,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShowWhen {
    Always,
    WhenRunning,
    WhenStopped,
    WhenScrolled,
}

impl ShowWhen {
    pub fn visible(self, process_state: State, scrolled_back: bool) -> bool {
        match self {
            Self::Always => true,
            Self::WhenRunning => process_state == State::Running,
            Self::WhenStopped => matches!(process_state, State::Stopped | State::Failed),
            Self::WhenScrolled => scrolled_back,
        }
    }
}

pub struct KeyHint {
    pub keys: &'static str,
    pub desc: &'static str,
    pub category: Category,
    pub bar: Option<(&'static str, ShowWhen)>,
    pub right: bool,
}

impl KeyHint {
    const fn new(keys: &'static str, desc: &'static str, category: Category) -> Self {
        Self {
            keys,
            desc,
            category,
            bar: None,
            right: false,
        }
    }

    const fn bar(mut self, keys: &'static str, when: ShowWhen) -> Self {
        self.bar = Some((keys, when));
        self
    }

    const fn right(mut self) -> Self {
        self.right = true;
        self
    }

    pub fn help_width(&self) -> u16 {
        (2 + self.keys.chars().count() + 1 + self.desc.chars().count()) as u16
    }
}

pub fn normal_hints() -> &'static [KeyHint] {
    use Category::*;
    use ShowWhen::*;
    const HINTS: &[KeyHint] = &[
        KeyHint::new("h/l ←/→ tab", "tabs", Navigation).bar("h/l", Always),
        KeyHint::new("1-9", "go to tab", Navigation),
        KeyHint::new("j/k ↑/↓", "scroll", Navigation),
        KeyHint::new("ctrl-f/b", "scroll pages", Navigation).bar("^f/^b", WhenScrolled),
        KeyHint::new("pgdn/pgup", "scroll pages", Navigation),
        KeyHint::new("home/end", "top/bottom", Navigation),
        KeyHint::new("i/enter", "focus", Actions).bar("i", WhenRunning),
        KeyHint::new("s", "start", Actions).bar("s", WhenStopped),
        KeyHint::new("x", "stop", Actions).bar("x", WhenRunning),
        KeyHint::new("r", "restart", Actions).bar("r", WhenRunning),
        KeyHint::new("/", "search", Navigation).bar("/", Always),
        KeyHint::new("n/N", "next/prev match", Navigation),
        KeyHint::new("q", "quit", App).bar("q", Always).right(),
        KeyHint::new("Q", "force quit", App),
        KeyHint::new("?", "help", App).bar("?", Always).right(),
    ];
    HINTS
}

pub fn focused_hints() -> &'static [KeyHint] {
    use Category::*;
    use ShowWhen::*;
    const HINTS: &[KeyHint] = &[KeyHint::new("esc", "unfocus", Actions).bar("esc", Always)];
    HINTS
}
