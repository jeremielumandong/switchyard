//! SQL editor commands the code editor does not have: toggle line and block comments,
//! upper / lower case, duplicate, move and delete lines (DBeaver's editor shortcuts).
//!
//! Each command is a pure function from the text and selection (byte offsets) to one
//! [`Edit`], applied as a single undo step by [`apply`].

use std::ops::Range;

use gpui_kit::component::input::EditorState;
use gpui_kit::{Context, Window};

/// One replacement and the selection after it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    /// Bytes replaced.
    pub range: Range<usize>,
    /// Replacement text.
    pub text: String,
    /// Selection afterwards (absolute, in the edited text).
    pub select: Range<usize>,
}

/// Apply `edit` to the editor as one undoable change.
pub fn apply(e: &mut EditorState, edit: Edit, window: &mut Window, cx: &mut Context<EditorState>) {
    e.set_selected_range(edit.range, cx);
    e.replace(edit.text, window, cx);
    e.set_selected_range(edit.select, cx);
}

/// The whole lines the selection touches: from the start of the first line to the end of
/// the last (without its newline). A selection ending at the start of a line leaves that
/// line out, as editors do.
fn line_block(text: &str, sel: &Range<usize>) -> Range<usize> {
    let start = text[..sel.start].rfind('\n').map_or(0, |i| i + 1);
    let mut end_at = sel.end;
    if sel.end > sel.start && sel.end > start && text[..sel.end].ends_with('\n') {
        end_at -= 1;
    }
    let end = text[end_at..].find('\n').map_or(text.len(), |i| end_at + i);
    start..end.max(start)
}

/// Comment out the selected lines with `prefix` (`--`), or uncomment them when every
/// non-blank line already starts with it.
pub fn toggle_line_comment(text: &str, sel: Range<usize>, prefix: &str) -> Edit {
    let block = line_block(text, &sel);
    let lines: Vec<&str> = text[block.clone()].split('\n').collect();
    let code: Vec<&&str> = lines.iter().filter(|l| !l.trim().is_empty()).collect();
    let commented = !code.is_empty() && code.iter().all(|l| l.trim_start().starts_with(prefix));
    let indent = code
        .iter()
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    let mut out = String::with_capacity(block.len() + lines.len() * (prefix.len() + 1));
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if line.trim().is_empty() {
            out.push_str(line);
        } else if commented {
            let lead = line.len() - line.trim_start().len();
            let rest = &line[lead + prefix.len()..];
            out.push_str(&line[..lead]);
            out.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        } else {
            out.push_str(&line[..indent]);
            out.push_str(prefix);
            out.push(' ');
            out.push_str(&line[indent..]);
        }
    }
    let select = if sel.is_empty() {
        // One line: keep the caret on the same character.
        let col = sel.start - block.start;
        let line = lines[0];
        let lead = line.len() - line.trim_start().len();
        let c = if line.trim().is_empty() {
            col
        } else if commented {
            let removed = (line.len() - lead) - (out.len() - lead);
            if col >= lead + removed {
                col - removed
            } else {
                col.min(lead)
            }
        } else if col >= indent {
            col + prefix.len() + 1
        } else {
            col
        };
        block.start + c..block.start + c
    } else {
        // Keep whole-line selections covering the edited lines.
        block.start..block.start + out.len()
    };
    Edit {
        range: block,
        text: out,
        select,
    }
}

/// Wrap the selection (or the current line) in `/* … */`, or unwrap it when it already is.
pub fn toggle_block_comment(text: &str, sel: Range<usize>) -> Edit {
    let range = if sel.is_empty() {
        let b = line_block(text, &sel);
        let line = &text[b.clone()];
        let lead = line.len() - line.trim_start().len();
        let trail = line.len() - line.trim_end().len();
        b.start + lead..b.end - trail
    } else {
        sel
    };
    let inner = &text[range.clone()];
    let new = match inner.strip_prefix("/*").and_then(|s| s.strip_suffix("*/")) {
        Some(body) => body
            .strip_prefix(' ')
            .and_then(|b| b.strip_suffix(' '))
            .unwrap_or(body)
            .to_owned(),
        None => format!("/* {inner} */"),
    };
    let select = range.start..range.start + new.len();
    Edit {
        range,
        text: new,
        select,
    }
}

/// The word around `at` (letters, digits, `_`), or `at..at`.
fn word_at(text: &str, at: usize) -> Range<usize> {
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let start = text[..at]
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_word(*c))
        .last()
        .map_or(at, |(i, _)| i);
    let end = text[at..]
        .char_indices()
        .find(|(_, c)| !is_word(*c))
        .map_or(text.len(), |(i, _)| at + i);
    start..end
}

/// Upper- or lower-case the selection, or the word at the cursor.
pub fn change_case(text: &str, sel: Range<usize>, upper: bool) -> Edit {
    let range = if sel.is_empty() {
        word_at(text, sel.start)
    } else {
        sel.clone()
    };
    let s = &text[range.clone()];
    let new = if upper {
        s.to_uppercase()
    } else {
        s.to_lowercase()
    };
    let select = if sel.is_empty() {
        sel
    } else {
        range.start..range.start + new.len()
    };
    Edit {
        range,
        text: new,
        select,
    }
}

/// Duplicate the selection after itself, or the current lines below them.
pub fn duplicate(text: &str, sel: Range<usize>) -> Edit {
    if !sel.is_empty() {
        let copy = text[sel.clone()].to_owned();
        let at = sel.end;
        return Edit {
            range: at..at,
            select: at..at + copy.len(),
            text: copy,
        };
    }
    let block = line_block(text, &sel);
    let copy = format!("\n{}", &text[block.clone()]);
    let at = block.end;
    let caret = sel.start + copy.len();
    Edit {
        range: at..at,
        text: copy,
        select: caret..caret,
    }
}

/// Move the selected lines one line up or down, or `None` at the top / bottom.
pub fn move_lines(text: &str, sel: Range<usize>, up: bool) -> Option<Edit> {
    let block = line_block(text, &sel);
    let lines = text[block.clone()].to_owned();
    if up {
        if block.start == 0 {
            return None;
        }
        let prev_start = text[..block.start - 1].rfind('\n').map_or(0, |i| i + 1);
        let prev = &text[prev_start..block.start - 1];
        let new = format!("{lines}\n{prev}");
        let shift = prev.len() + 1;
        Some(Edit {
            range: prev_start..block.end,
            text: new,
            select: sel.start - shift..sel.end - shift,
        })
    } else {
        if block.end >= text.len() {
            return None;
        }
        let next_end = text[block.end + 1..]
            .find('\n')
            .map_or(text.len(), |i| block.end + 1 + i);
        let next = &text[block.end + 1..next_end];
        let new = format!("{next}\n{lines}");
        let shift = next.len() + 1;
        Some(Edit {
            range: block.start..next_end,
            text: new,
            select: sel.start + shift..sel.end + shift,
        })
    }
}

/// Delete the selected lines (with their newline).
pub fn delete_lines(text: &str, sel: Range<usize>) -> Edit {
    let block = line_block(text, &sel);
    let range = if block.end < text.len() {
        block.start..block.end + 1
    } else if block.start > 0 {
        block.start - 1..block.end
    } else {
        block.clone()
    };
    let caret = range.start.min(text.len() - range.len());
    let caret = if block.end < text.len() || block.start == 0 {
        caret
    } else {
        text[..range.start].rfind('\n').map_or(0, |i| i + 1)
    };
    Edit {
        range,
        text: String::new(),
        select: caret..caret,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Apply an edit to a string.
    fn run(text: &str, e: &Edit) -> String {
        format!(
            "{}{}{}",
            &text[..e.range.start],
            e.text,
            &text[e.range.end..]
        )
    }

    #[test]
    fn line_comments_toggle() {
        let t = "select 1\n  from t\n\nwhere x";
        // Cursor on line 2: only that line, at its indentation.
        let e = toggle_line_comment(t, 12..12, "--");
        assert_eq!(run(t, &e), "select 1\n  -- from t\n\nwhere x");
        assert_eq!(e.select, 15..15, "the caret stays on the same character");
        // A selection over all lines comments each non-blank one at the common indent.
        let e = toggle_line_comment(t, 0..t.len(), "--");
        let out = run(t, &e);
        assert_eq!(out, "-- select 1\n--   from t\n\n-- where x");
        // And toggles back.
        let back = toggle_line_comment(&out, 0..out.len(), "--");
        assert_eq!(run(&out, &back), t);
        // A selection ending at the start of a line leaves that line alone.
        let e = toggle_line_comment(t, 0..9, "--");
        assert_eq!(run(t, &e), "-- select 1\n  from t\n\nwhere x");
        // `--x` without a space uncomments too.
        let e = toggle_line_comment("--x", 0..0, "--");
        assert_eq!(run("--x", &e), "x");
    }

    #[test]
    fn block_comments_toggle() {
        let t = "select a, b from t";
        let e = toggle_block_comment(t, 7..11);
        let out = run(t, &e);
        assert_eq!(out, "select /* a, b */ from t");
        let back = toggle_block_comment(&out, e.select.clone());
        assert_eq!(run(&out, &back), t);
        // No selection: the current line without its indentation.
        let e = toggle_block_comment("  x = 1\ny", 3..3);
        assert_eq!(run("  x = 1\ny", &e), "  /* x = 1 */\ny");
    }

    #[test]
    fn case_changes() {
        let t = "select name from t";
        let e = change_case(t, 0..6, true);
        assert_eq!(run(t, &e), "SELECT name from t");
        assert_eq!(e.select, 0..6);
        // No selection: the word at the cursor, caret kept.
        let e = change_case(t, 9..9, true);
        assert_eq!(run(t, &e), "select NAME from t");
        assert_eq!(e.select, 9..9);
        let e = change_case("ABC", 0..3, false);
        assert_eq!(e.text, "abc");
    }

    #[test]
    fn duplicates() {
        let t = "a\nbc\nd";
        let e = duplicate(t, 3..3);
        assert_eq!(run(t, &e), "a\nbc\nbc\nd");
        assert_eq!(e.select, 6..6, "caret moves to the copy");
        let e = duplicate(t, 2..4);
        assert_eq!(run(t, &e), "a\nbcbc\nd");
    }

    #[test]
    fn moves_lines() {
        let t = "a\nbc\nd";
        let e = move_lines(t, 3..3, true).unwrap();
        assert_eq!(run(t, &e), "bc\na\nd");
        assert_eq!(e.select, 1..1);
        let e = move_lines(t, 3..3, false).unwrap();
        assert_eq!(run(t, &e), "a\nd\nbc");
        assert_eq!(e.select, 5..5);
        assert!(move_lines(t, 0..0, true).is_none());
        assert!(move_lines(t, 6..6, false).is_none());
        // Two selected lines move together.
        let e = move_lines(t, 0..4, false).unwrap();
        assert_eq!(run(t, &e), "d\na\nbc");
    }

    #[test]
    fn deletes_lines() {
        let t = "a\nbc\nd";
        let e = delete_lines(t, 3..3);
        assert_eq!(run(t, &e), "a\nd");
        assert_eq!(e.select, 2..2);
        let e = delete_lines(t, 6..6);
        assert_eq!(run(t, &e), "a\nbc");
        assert_eq!(e.select, 2..2);
        let e = delete_lines("x", 0..0);
        assert_eq!(run("x", &e), "");
    }
}
