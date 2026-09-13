//! Which key events count as typed text rather than shortcuts.

use crossterm::event::{KeyEvent, KeyModifiers};

/// Whether a `KeyCode::Char` event belongs in a filter.
///
/// Alt is a shortcut prefix in terminal applications, so Alt+d must not type
/// a `d`. The filters used to accept it because they only tested for Ctrl.
///
/// AltGr arrives as Ctrl+Alt on Windows. The filters have never accepted it,
/// and this keeps that unchanged rather than guessing at keyboard layouts
/// that cannot be tested here.
pub fn is_typed_text(key: &KeyEvent) -> bool {
    !key.modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyCode;

    #[test]
    fn shift_is_text_but_alt_and_ctrl_are_not() {
        let plain = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE);
        let shifted = KeyEvent::new(KeyCode::Char('D'), KeyModifiers::SHIFT);
        let alt = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::ALT);
        let ctrl = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
        let altgr = KeyEvent::new(
            KeyCode::Char('@'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        );

        assert!(is_typed_text(&plain));
        assert!(is_typed_text(&shifted));
        assert!(!is_typed_text(&alt));
        assert!(!is_typed_text(&ctrl));
        assert!(!is_typed_text(&altgr));
    }
}
