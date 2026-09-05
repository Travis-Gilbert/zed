use std::ops::Range;

/// An IME edit relative to the mirror's pre-edit selection, in UTF-16 units.
/// The caller resolves the current editor selection immediately before applying
/// these distances, so a concurrent document change cannot stale the position.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct InputEdit {
    pub removed_before_selection: usize,
    pub removed_after_selection: usize,
    pub inserted_text: String,
}

pub(crate) fn input_edit(
    old_value: &str,
    new_value: &str,
    selection: Range<usize>,
    post_edit_caret: Option<usize>,
) -> InputEdit {
    let old_units: Vec<u16> = old_value.encode_utf16().collect();
    let new_units: Vec<u16> = new_value.encode_utf16().collect();

    // The caret disambiguates repeated text: inserting "pactor " before
    // "pact" must not become an insertion of "or pact" four units later.
    // Only text outside the pre-edit selection may be trimmed. Matching text
    // inside that selection is still part of the replacement sent to GPUI.
    let max_suffix = old_units.len().saturating_sub(selection.end);
    let anchored_suffix = post_edit_caret
        .filter(|&caret| caret <= new_units.len())
        .map(|caret| new_units.len() - caret)
        .filter(|&length| {
            length <= max_suffix
                && old_units[old_units.len() - length..] == new_units[new_units.len() - length..]
        });
    let mut suffix = anchored_suffix.unwrap_or_else(|| {
        old_units
            .iter()
            .rev()
            .zip(new_units.iter().rev())
            .take_while(|(old, new)| old == new)
            .count()
            .min(max_suffix)
    });
    if splits_surrogate_pair(&old_units, old_units.len() - suffix)
        || splits_surrogate_pair(&new_units, new_units.len() - suffix)
    {
        suffix -= 1;
    }
    let mut prefix = old_units
        .iter()
        .zip(&new_units)
        .take_while(|(old, new)| old == new)
        .count()
        .min(selection.start)
        .min(old_units.len() - suffix)
        .min(new_units.len() - suffix);
    if splits_surrogate_pair(&old_units, prefix) || splits_surrogate_pair(&new_units, prefix) {
        prefix -= 1;
    }

    InputEdit {
        removed_before_selection: selection.start.saturating_sub(prefix),
        removed_after_selection: (old_units.len() - suffix).saturating_sub(selection.end),
        inserted_text: String::from_utf16_lossy(&new_units[prefix..new_units.len() - suffix]),
    }
}

fn splits_surrogate_pair(units: &[u16], offset: usize) -> bool {
    offset > 0
        && offset < units.len()
        && (0xd800..=0xdbff).contains(&units[offset - 1])
        && (0xdc00..=0xdfff).contains(&units[offset])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(old: &str, new: &str, selection: Range<usize>, caret: Option<usize>, expected: &str) {
        let edit = input_edit(old, new, selection.clone(), caret);
        assert_eq!(edit.inserted_text, expected);
        let mut units: Vec<u16> = old.encode_utf16().collect();
        units.splice(
            selection.start - edit.removed_before_selection
                ..selection.end + edit.removed_after_selection,
            edit.inserted_text.encode_utf16(),
        );
        assert_eq!(String::from_utf16(&units).unwrap(), new);
    }

    #[test]
    fn selected_replacement_preserves_its_matching_prefix_and_suffix() {
        check(
            "original",
            "original\nadded",
            0..8,
            Some(14),
            "original\nadded",
        );
        check("original", "new original", 0..8, Some(12), "new original");
        check("a topic z", "a topical z", 2..7, Some(9), "topical");
        check("a topic z", "a topical z", 2..7, None, "topical");
    }

    #[test]
    fn caret_disambiguates_insertion_before_repeated_text() {
        check("pact", "pactor pact", 0..0, Some(7), "pactor ");
        check("aaaa", "aaaaa", 2..2, Some(3), "a");
    }

    #[test]
    fn deletion_and_autocorrection_extend_the_current_selection() {
        check("word!", "wor!", 4..4, Some(3), "");
        check("word!", "wod!", 2..2, Some(2), "");
        check("teh rest", "the rest", 3..3, Some(3), "he");
        check("hello world", "hello ", 6..11, Some(6), "");
    }

    #[test]
    fn utf16_edit_boundaries_never_split_surrogate_pairs() {
        check("a🙂b", "a🙃b", 3..3, Some(3), "🙃");
        check("a🙂b", "a🙂 addedb", 1..3, Some(9), "🙂 added");
        check("a🙂b", "ab", 3..3, Some(1), "");
    }

    #[test]
    fn contiguous_edits_reconstruct_text_at_fresh_editor_anchors() {
        let mut texts = vec![String::new()];
        for _ in 0..3 {
            let previous = texts.clone();
            for prefix in previous {
                for letter in ["a", "b", "🙂"] {
                    texts.push(format!("{prefix}{letter}"));
                }
            }
        }
        texts.sort();
        texts.dedup();
        for old in &texts {
            let mut boundaries = vec![0];
            for letter in old.chars() {
                boundaries.push(boundaries.last().unwrap() + letter.len_utf16());
            }
            let old_units: Vec<u16> = old.encode_utf16().collect();
            for &start in &boundaries {
                for &end in boundaries.iter().filter(|&&end| end >= start) {
                    for replacement in &texts {
                        let mut expected = old_units.clone();
                        expected.splice(start..end, replacement.encode_utf16());
                        let new = String::from_utf16(&expected).unwrap();
                        let edit = input_edit(
                            old,
                            &new,
                            start..end,
                            Some(start + replacement.encode_utf16().count()),
                        );
                        // A remote insertion shifted both selection anchors after
                        // the mirror sync. The edit contains no stale absolute offset.
                        let remote = "remote🙂 ";
                        let shift = remote.encode_utf16().count();
                        let mut actual: Vec<u16> =
                            remote.encode_utf16().chain(old.encode_utf16()).collect();
                        actual.splice(
                            shift + start - edit.removed_before_selection
                                ..shift + end + edit.removed_after_selection,
                            edit.inserted_text.encode_utf16(),
                        );
                        assert_eq!(
                            String::from_utf16(&actual).unwrap(),
                            format!("{remote}{new}")
                        );
                    }
                }
            }
        }
    }
}
