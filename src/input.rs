// Keybinding resolution, focus mode state machine.

use crate::config::UnfocusKey;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Focused,
    Search,
    ConfirmQuit,
    Quitting,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    SelectTab(usize),
    NextTab,
    PrevTab,
    Focus,
    Unfocus,
    StartProcess,
    StopProcess,
    RestartProcess,
    ScrollUp(ScrollAmount),
    ScrollDown(ScrollAmount),
    ScrollToTop,
    ScrollToBottom,
    EnterSearch,
    SearchAccept,
    SearchCancel,
    SearchInput(char),
    SearchBackspace,
    SearchFillPlaceholder,
    SearchNext,
    SearchPrev,
    ToggleHelp,
    Quit,
    ForceQuit,
    CancelQuit,
    ForwardKey(KeyEvent),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollAmount {
    Line,
    FullPage,
}

pub fn resolve(key: KeyEvent, mode: Mode, unfocus_key: &UnfocusKey) -> Option<Action> {
    match mode {
        Mode::Focused => resolve_focused(key, unfocus_key),
        Mode::Normal => resolve_normal(key),
        Mode::Search => resolve_search(key),
        Mode::ConfirmQuit => resolve_confirm_quit(key),
        Mode::Quitting => resolve_quitting(key),
    }
}

fn resolve_quitting(key: KeyEvent) -> Option<Action> {
    if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
        return Some(Action::ForceQuit);
    }
    None
}

fn resolve_confirm_quit(key: KeyEvent) -> Option<Action> {
    match key.code {
        KeyCode::Char('y') if key.modifiers.is_empty() => Some(Action::Quit),
        KeyCode::Enter => Some(Action::Quit),
        _ => Some(Action::CancelQuit),
    }
}

fn resolve_focused(key: KeyEvent, unfocus_key: &UnfocusKey) -> Option<Action> {
    if matches_unfocus_key(&key, unfocus_key) {
        return Some(Action::Unfocus);
    }
    Some(Action::ForwardKey(key))
}

fn matches_unfocus_key(key: &KeyEvent, unfocus_key: &UnfocusKey) -> bool {
    match unfocus_key {
        UnfocusKey::Esc => key.code == KeyCode::Esc && key.modifiers.is_empty(),
        UnfocusKey::Char(c) => key.code == KeyCode::Char(*c) && key.modifiers.is_empty(),
        UnfocusKey::Ctrl(c) => {
            key.code == KeyCode::Char(*c) && key.modifiers == KeyModifiers::CONTROL
        }
    }
}

fn resolve_search(key: KeyEvent) -> Option<Action> {
    if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
        return Some(Action::SearchCancel);
    }
    let mods = key.modifiers - KeyModifiers::SHIFT;
    if !mods.is_empty() {
        return None;
    }
    match key.code {
        KeyCode::Enter => Some(Action::SearchAccept),
        KeyCode::Esc => Some(Action::SearchCancel),
        KeyCode::Tab => Some(Action::SearchFillPlaceholder),
        KeyCode::Backspace => Some(Action::SearchBackspace),
        KeyCode::Char(c) => Some(Action::SearchInput(c)),
        _ => None,
    }
}

fn resolve_normal(key: KeyEvent) -> Option<Action> {
    if !key.modifiers.is_empty() {
        return resolve_normal_modified(key);
    }
    match key.code {
        KeyCode::Char(c @ '1'..='9') => Some(Action::SelectTab(c as usize - '1' as usize)),
        KeyCode::Char('l') | KeyCode::Right | KeyCode::Tab => Some(Action::NextTab),
        KeyCode::Char('h') | KeyCode::Left => Some(Action::PrevTab),
        KeyCode::Char('i') | KeyCode::Enter => Some(Action::Focus),
        KeyCode::Char('s') => Some(Action::StartProcess),
        KeyCode::Char('x') => Some(Action::StopProcess),
        KeyCode::Char('r') => Some(Action::RestartProcess),
        KeyCode::Char('j') | KeyCode::Down => Some(Action::ScrollDown(ScrollAmount::Line)),
        KeyCode::Char('k') | KeyCode::Up => Some(Action::ScrollUp(ScrollAmount::Line)),
        KeyCode::PageUp => Some(Action::ScrollUp(ScrollAmount::FullPage)),
        KeyCode::PageDown => Some(Action::ScrollDown(ScrollAmount::FullPage)),
        KeyCode::Home => Some(Action::ScrollToTop),
        KeyCode::End => Some(Action::ScrollToBottom),
        KeyCode::Char('/') => Some(Action::EnterSearch),
        KeyCode::Char('n') => Some(Action::SearchNext),
        KeyCode::Esc => Some(Action::SearchCancel),
        KeyCode::Char('q') => Some(Action::Quit),
        KeyCode::Char('?') => Some(Action::ToggleHelp),
        _ => None,
    }
}

fn resolve_normal_modified(key: KeyEvent) -> Option<Action> {
    if key.modifiers == KeyModifiers::SHIFT {
        match key.code {
            KeyCode::Char('Q') => return Some(Action::ForceQuit),
            KeyCode::Char('N') => return Some(Action::SearchPrev),
            // Some terminals report ? with SHIFT modifier, others without.
            KeyCode::Char('?') => return Some(Action::ToggleHelp),
            KeyCode::BackTab => return Some(Action::PrevTab),
            _ => {}
        }
    }
    if key.modifiers == KeyModifiers::CONTROL {
        match key.code {
            KeyCode::Char('b') => return Some(Action::ScrollUp(ScrollAmount::FullPage)),
            KeyCode::Char('c') => return Some(Action::Quit),
            KeyCode::Char('f') => return Some(Action::ScrollDown(ScrollAmount::FullPage)),
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn normal_tab_selection() {
        assert_eq!(
            resolve(key(KeyCode::Char('1')), Mode::Normal, &UnfocusKey::Esc),
            Some(Action::SelectTab(0))
        );
        assert_eq!(
            resolve(key(KeyCode::Char('9')), Mode::Normal, &UnfocusKey::Esc),
            Some(Action::SelectTab(8))
        );
    }

    #[test]
    fn normal_tab_navigation() {
        assert_eq!(
            resolve(key(KeyCode::Char('l')), Mode::Normal, &UnfocusKey::Esc),
            Some(Action::NextTab)
        );
        assert_eq!(
            resolve(key(KeyCode::Right), Mode::Normal, &UnfocusKey::Esc),
            Some(Action::NextTab)
        );
        assert_eq!(
            resolve(key(KeyCode::Char('h')), Mode::Normal, &UnfocusKey::Esc),
            Some(Action::PrevTab)
        );
        assert_eq!(
            resolve(key(KeyCode::Left), Mode::Normal, &UnfocusKey::Esc),
            Some(Action::PrevTab)
        );
    }

    #[test]
    fn normal_focus_and_quit() {
        assert_eq!(
            resolve(key(KeyCode::Char('i')), Mode::Normal, &UnfocusKey::Esc),
            Some(Action::Focus)
        );
        assert_eq!(
            resolve(key(KeyCode::Enter), Mode::Normal, &UnfocusKey::Esc),
            Some(Action::Focus)
        );
        assert_eq!(
            resolve(key(KeyCode::Char('q')), Mode::Normal, &UnfocusKey::Esc),
            Some(Action::Quit)
        );
    }

    #[test]
    fn normal_process_control() {
        assert_eq!(
            resolve(key(KeyCode::Char('s')), Mode::Normal, &UnfocusKey::Esc),
            Some(Action::StartProcess)
        );
        assert_eq!(
            resolve(key(KeyCode::Char('x')), Mode::Normal, &UnfocusKey::Esc),
            Some(Action::StopProcess)
        );
        assert_eq!(
            resolve(key(KeyCode::Char('r')), Mode::Normal, &UnfocusKey::Esc),
            Some(Action::RestartProcess)
        );
    }

    #[test]
    fn normal_scrolling() {
        assert_eq!(
            resolve(key(KeyCode::Char('k')), Mode::Normal, &UnfocusKey::Esc),
            Some(Action::ScrollUp(ScrollAmount::Line))
        );
        assert_eq!(
            resolve(key(KeyCode::Up), Mode::Normal, &UnfocusKey::Esc),
            Some(Action::ScrollUp(ScrollAmount::Line))
        );
        assert_eq!(
            resolve(key(KeyCode::Char('j')), Mode::Normal, &UnfocusKey::Esc),
            Some(Action::ScrollDown(ScrollAmount::Line))
        );
        assert_eq!(
            resolve(ctrl('b'), Mode::Normal, &UnfocusKey::Esc),
            Some(Action::ScrollUp(ScrollAmount::FullPage))
        );
        assert_eq!(
            resolve(ctrl('f'), Mode::Normal, &UnfocusKey::Esc),
            Some(Action::ScrollDown(ScrollAmount::FullPage))
        );
        assert_eq!(
            resolve(key(KeyCode::PageUp), Mode::Normal, &UnfocusKey::Esc),
            Some(Action::ScrollUp(ScrollAmount::FullPage))
        );
        assert_eq!(
            resolve(key(KeyCode::PageDown), Mode::Normal, &UnfocusKey::Esc),
            Some(Action::ScrollDown(ScrollAmount::FullPage))
        );
    }

    #[test]
    fn normal_unbound_key_returns_none() {
        assert_eq!(
            resolve(key(KeyCode::Char('z')), Mode::Normal, &UnfocusKey::Esc),
            None
        );
    }

    #[test]
    fn focused_esc_unfocuses() {
        assert_eq!(
            resolve(key(KeyCode::Esc), Mode::Focused, &UnfocusKey::Esc),
            Some(Action::Unfocus)
        );
    }

    #[test]
    fn focused_forwards_other_keys() {
        let k = key(KeyCode::Char('a'));
        assert_eq!(
            resolve(k, Mode::Focused, &UnfocusKey::Esc),
            Some(Action::ForwardKey(k))
        );
    }

    #[test]
    fn focused_custom_unfocus_key() {
        let unfocus = UnfocusKey::Ctrl('c');

        // ctrl-c should unfocus
        assert_eq!(
            resolve(ctrl('c'), Mode::Focused, &unfocus),
            Some(Action::Unfocus)
        );

        // esc should be forwarded when unfocus key is ctrl-c
        let esc = key(KeyCode::Esc);
        assert_eq!(
            resolve(esc, Mode::Focused, &unfocus),
            Some(Action::ForwardKey(esc))
        );
    }

    #[test]
    fn confirm_quit_y_quits() {
        assert_eq!(
            resolve(key(KeyCode::Char('y')), Mode::ConfirmQuit, &UnfocusKey::Esc),
            Some(Action::Quit)
        );
        assert_eq!(
            resolve(key(KeyCode::Enter), Mode::ConfirmQuit, &UnfocusKey::Esc),
            Some(Action::Quit)
        );
    }

    #[test]
    fn confirm_quit_other_cancels() {
        assert_eq!(
            resolve(key(KeyCode::Char('n')), Mode::ConfirmQuit, &UnfocusKey::Esc),
            Some(Action::CancelQuit)
        );
        assert_eq!(
            resolve(key(KeyCode::Esc), Mode::ConfirmQuit, &UnfocusKey::Esc),
            Some(Action::CancelQuit)
        );
    }

    #[test]
    fn quitting_ctrl_c_force_quits() {
        assert_eq!(
            resolve(ctrl('c'), Mode::Quitting, &UnfocusKey::Esc),
            Some(Action::ForceQuit)
        );
    }

    #[test]
    fn quitting_ignores_other_keys() {
        assert_eq!(
            resolve(key(KeyCode::Char('q')), Mode::Quitting, &UnfocusKey::Esc),
            None
        );
    }

    #[test]
    fn search_ctrl_c_cancels() {
        assert_eq!(
            resolve(ctrl('c'), Mode::Search, &UnfocusKey::Esc),
            Some(Action::SearchCancel)
        );
    }
}
