// Single source of truth for keybinding hints (status bar + help panel).

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

pub struct KeyHint {
    pub keys: &'static str,
    pub desc: &'static str,
    pub category: Category,
    pub bar: Option<&'static str>,
    pub bar_when: ShowWhen,
}

impl KeyHint {
    pub fn help_width(&self) -> u16 {
        (2 + self.keys.chars().count() + 1 + self.desc.chars().count()) as u16
    }
}

pub fn normal_hints() -> &'static [KeyHint] {
    &[
        KeyHint {
            keys: "h/l ←/→ tab",
            desc: "tabs",
            category: Category::Navigation,
            bar: Some("h/l"),
            bar_when: ShowWhen::Always,
        },
        KeyHint {
            keys: "1-9",
            desc: "go to tab",
            category: Category::Navigation,
            bar: None,
            bar_when: ShowWhen::Always,
        },
        KeyHint {
            keys: "j/k ↑/↓",
            desc: "scroll",
            category: Category::Navigation,
            bar: Some("j/k"),
            bar_when: ShowWhen::WhenScrolled,
        },
        KeyHint {
            keys: "ctrl-f/b",
            desc: "scroll pages",
            category: Category::Navigation,
            bar: None,
            bar_when: ShowWhen::Always,
        },
        KeyHint {
            keys: "pgdn/pgup",
            desc: "scroll pages",
            category: Category::Navigation,
            bar: None,
            bar_when: ShowWhen::Always,
        },
        KeyHint {
            keys: "home/end",
            desc: "top/bottom",
            category: Category::Navigation,
            bar: None,
            bar_when: ShowWhen::Always,
        },
        KeyHint {
            keys: "i/enter",
            desc: "focus",
            category: Category::Actions,
            bar: Some("i"),
            bar_when: ShowWhen::WhenRunning,
        },
        KeyHint {
            keys: "s",
            desc: "start",
            category: Category::Actions,
            bar: Some("s"),
            bar_when: ShowWhen::WhenStopped,
        },
        KeyHint {
            keys: "x",
            desc: "stop",
            category: Category::Actions,
            bar: Some("x"),
            bar_when: ShowWhen::WhenRunning,
        },
        KeyHint {
            keys: "r",
            desc: "restart",
            category: Category::Actions,
            bar: Some("r"),
            bar_when: ShowWhen::WhenRunning,
        },
        KeyHint {
            keys: "/",
            desc: "search",
            category: Category::Navigation,
            bar: Some("/"),
            bar_when: ShowWhen::Always,
        },
        KeyHint {
            keys: "n/N",
            desc: "next/prev match",
            category: Category::Navigation,
            bar: None,
            bar_when: ShowWhen::Always,
        },
        KeyHint {
            keys: "q",
            desc: "quit",
            category: Category::App,
            bar: Some("q"),
            bar_when: ShowWhen::Always,
        },
        KeyHint {
            keys: "Q",
            desc: "force quit",
            category: Category::App,
            bar: None,
            bar_when: ShowWhen::Always,
        },
        KeyHint {
            keys: "?",
            desc: "help",
            category: Category::App,
            bar: Some("?"),
            bar_when: ShowWhen::Always,
        },
    ]
}

pub fn focused_hints() -> &'static [KeyHint] {
    &[KeyHint {
        keys: "esc",
        desc: "unfocus",
        category: Category::Actions,
        bar: Some("esc"),
        bar_when: ShowWhen::Always,
    }]
}
