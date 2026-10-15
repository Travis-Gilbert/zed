//! Keys whose GPUI bindings can mutate the composing editor or its focus.
//! The platform composition owns those changes until commit/cancel. Copy and
//! unrelated application shortcuts keep their existing keybinding path.

pub(crate) fn is_owned_key(key: &str, primary: bool, alt: bool) -> bool {
    matches!(
        key,
        "enter"
            | "escape"
            | "tab"
            | "backspace"
            | "delete"
            | "left"
            | "right"
            | "up"
            | "down"
            | "home"
            | "end"
            | "pageup"
            | "pagedown"
    ) || (primary && !alt && matches!(key, "a" | "x" | "v" | "z" | "y"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composing_editor_keeps_submit_cancel_focus_selection_and_deletion() {
        for key in [
            "enter",
            "escape",
            "tab",
            "backspace",
            "delete",
            "left",
            "right",
            "up",
            "down",
            "home",
            "end",
            "pageup",
            "pagedown",
        ] {
            assert!(is_owned_key(key, false, false), "{key}");
            assert!(is_owned_key(key, true, true), "modified {key}");
        }
    }

    #[test]
    fn composing_editor_keeps_its_cut_paste_selection_and_history_bindings() {
        for key in ["a", "x", "v", "z", "y"] {
            assert!(is_owned_key(key, true, false), "primary {key}");
            assert!(!is_owned_key(key, false, false), "plain {key}");
            assert!(!is_owned_key(key, true, true), "unbound alternate {key}");
        }
    }

    #[test]
    fn copy_and_unrelated_shortcuts_keep_their_existing_dispatch() {
        for key in ["c", "f", "r", "p", "F6", "Unidentified"] {
            assert!(!is_owned_key(key, true, false), "{key}");
            assert!(!is_owned_key(key, false, false), "plain {key}");
        }
    }
}
