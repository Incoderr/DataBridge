//! Line-level editing commands of the SQL editor (toggle comment,
//! duplicate / delete / move lines, new line above / below), the ones
//! gpui-component's editor does not ship. Pure text in, one replacement
//! out: every command maps `(text, selection)` to an [`Edit`] — one byte
//! range to replace and the selection afterwards — so the editor applies
//! it as a single undo step (`apply`). Offsets are byte offsets, as in the
//! editor's rope.

use std::ops::Range;

use gpui::{Context, Entity, Window};
use gpui_component::input::EditorState;

/// One replacement: `range` of the old text becomes `text`; `sel` is the
/// selection in the new text.
#[derive(Debug, Clone, PartialEq)]
pub struct Edit {
    pub range: Range<usize>,
    pub text: String,
    pub sel: Range<usize>,
}

/// Apply an edit to the editor as one undo step.
pub fn apply<T: 'static>(editor: &Entity<EditorState>, edit: Edit, window: &mut Window, cx: &mut Context<T>) {
    editor.update(cx, |s, cx| {
        s.set_selected_range(edit.range, cx);
        s.replace(edit.text, window, cx);
        s.set_selected_range(edit.sel, cx);
    });
}

/// Run a command against the editor's current text and selection.
pub fn run<T: 'static>(
    editor: &Entity<EditorState>,
    command: impl FnOnce(&str, Range<usize>) -> Option<Edit>,
    window: &mut Window,
    cx: &mut Context<T>,
) {
    let edit = {
        let s = editor.read(cx);
        let text = s.value();
        command(text.as_ref(), s.selected_range())
    };
    if let Some(edit) = edit {
        apply(editor, edit, window, cx);
    }
}

fn line_start(text: &str, off: usize) -> usize {
    text[..off].rfind('\n').map_or(0, |i| i + 1)
}

fn line_end(text: &str, off: usize) -> usize {
    text[off..].find('\n').map_or(text.len(), |i| off + i)
}

/// The whole lines a selection touches, without the final newline. A
/// selection ending at the start of a line does not take that line.
fn line_block(text: &str, sel: &Range<usize>) -> Range<usize> {
    let end = if sel.end > sel.start && text[..sel.end].ends_with('\n') { sel.end - 1 } else { sel.end };
    line_start(text, sel.start)..line_end(text, end.max(sel.start))
}

fn indent_of(line: &str) -> &str {
    &line[..line.len() - line.trim_start_matches([' ', '\t']).len()]
}

fn shift(r: &Range<usize>, by: isize) -> Range<usize> {
    (r.start as isize + by) as usize..(r.end as isize + by) as usize
}

/// Comment the touched lines out with `prefix` (`--`, or `//` for mongosh)
/// at their common indent — or, when every non-blank line is already
/// commented, remove the prefix (and one space after it).
pub fn toggle_comment(text: &str, sel: Range<usize>, prefix: &str) -> Option<Edit> {
    let block = line_block(text, &sel);
    let lines: Vec<&str> = text[block.clone()].split('\n').collect();
    let filled = lines.iter().filter(|l| !l.trim().is_empty());
    let uncomment = filled.clone().count() > 0 && filled.clone().all(|l| l.trim_start().starts_with(prefix));
    let indent = filled.map(|l| indent_of(l).len()).min().unwrap_or(0);

    // Per line: (offset in the old text, bytes removed, bytes inserted).
    let mut edits: Vec<(usize, usize, usize)> = Vec::new();
    let mut out = String::new();
    let mut pos = block.start;
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if uncomment && !line.trim().is_empty() {
            let at = indent_of(line).len();
            let mut cut = prefix.len();
            if line[at + cut..].starts_with(' ') {
                cut += 1;
            }
            out.push_str(&line[..at]);
            out.push_str(&line[at + cut..]);
            edits.push((pos + at, cut, 0));
        } else if !uncomment && (!line.trim().is_empty() || lines.len() == 1) {
            let at = indent.min(line.len());
            out.push_str(&line[..at]);
            out.push_str(prefix);
            out.push(' ');
            out.push_str(&line[at..]);
            edits.push((pos + at, 0, prefix.len() + 1));
        } else {
            out.push_str(line);
        }
        pos += line.len() + 1;
    }
    // `sticky`: an offset right at an insertion point stays before it
    // (a selection's start keeps covering the new prefix).
    let map = |off: usize, sticky: bool| -> usize {
        let mut new = off as isize;
        for &(at, removed, inserted) in &edits {
            if removed > 0 && off > at {
                new -= (off - at).min(removed) as isize;
            }
            if inserted > 0 && (off > at || (off == at && !sticky)) {
                new += inserted as isize;
            }
        }
        new as usize
    };
    let sel = if sel.is_empty() {
        let c = map(sel.start, false);
        c..c
    } else {
        map(sel.start, true)..map(sel.end, false)
    };
    Some(Edit { range: block, text: out, sel })
}

/// Duplicate the selection (right after it, selecting the copy) or, with
/// no selection, the current line(s) below, keeping the cursor's column.
pub fn duplicate(text: &str, sel: Range<usize>) -> Option<Edit> {
    if !sel.is_empty() {
        let copy = text[sel.clone()].to_string();
        let len = copy.len();
        return Some(Edit { range: sel.end..sel.end, text: copy, sel: sel.end..sel.end + len });
    }
    let block = line_block(text, &sel);
    let copy = format!("\n{}", &text[block.clone()]);
    let by = copy.len() as isize;
    Some(Edit { range: block.end..block.end, text: copy, sel: shift(&sel, by) })
}

/// Delete the touched lines; the cursor lands at the start of the line
/// that takes their place.
pub fn delete_lines(text: &str, sel: Range<usize>) -> Option<Edit> {
    let block = line_block(text, &sel);
    let (range, cursor) = if block.end < text.len() {
        (block.start..block.end + 1, block.start)
    } else if block.start > 0 {
        let prev = line_start(text, block.start - 1);
        (block.start - 1..block.end, prev)
    } else {
        (block.clone(), 0)
    };
    if range.is_empty() {
        return None;
    }
    Some(Edit { range, text: String::new(), sel: cursor..cursor })
}

/// Move the touched lines one line up or down (swap with the neighbour);
/// the selection moves with them. `None` at the edge of the text.
pub fn move_lines(text: &str, sel: Range<usize>, up: bool) -> Option<Edit> {
    let block = line_block(text, &sel);
    let lines = &text[block.clone()];
    if up {
        if block.start == 0 {
            return None;
        }
        let prev_start = line_start(text, block.start - 1);
        let prev = &text[prev_start..block.start - 1];
        Some(Edit {
            range: prev_start..block.end,
            text: format!("{lines}\n{prev}"),
            sel: shift(&sel, -(prev.len() as isize + 1)),
        })
    } else {
        if block.end >= text.len() {
            return None;
        }
        let next_end = line_end(text, block.end + 1);
        let next = &text[block.end + 1..next_end];
        Some(Edit {
            range: block.start..next_end,
            text: format!("{next}\n{lines}"),
            sel: shift(&sel, next.len() as isize + 1),
        })
    }
}

/// Start a new line below the current one (whatever the cursor's column),
/// with the current line's indent.
pub fn line_below(text: &str, sel: Range<usize>) -> Option<Edit> {
    let start = line_start(text, sel.end);
    let end = line_end(text, sel.end);
    let ins = format!("\n{}", indent_of(&text[start..end]));
    let c = end + ins.len();
    Some(Edit { range: end..end, text: ins, sel: c..c })
}

/// Start a new line above the current one, with its indent.
pub fn line_above(text: &str, sel: Range<usize>) -> Option<Edit> {
    let start = line_start(text, sel.start);
    let indent = indent_of(&text[start..line_end(text, sel.start)]).to_string();
    let c = start + indent.len();
    Some(Edit { range: start..start, text: format!("{indent}\n"), sel: c..c })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Apply `f` to `text` with the selection marked by `[` `]` (or a
    /// cursor `|`) and render the result the same way.
    fn check(f: impl Fn(&str, Range<usize>) -> Option<Edit>, before: &str, after: &str) {
        let (text, sel) = parse(before);
        let out = match f(&text, sel) {
            Some(e) => {
                let mut t = text.clone();
                t.replace_range(e.range, &e.text);
                render(&t, e.sel)
            }
            None => before.to_string(),
        };
        assert_eq!(out, after, "from {before:?}");
    }

    fn parse(s: &str) -> (String, Range<usize>) {
        if let Some(c) = s.find('|') {
            return (s.replacen('|', "", 1), c..c);
        }
        let a = s.find('[').unwrap();
        let b = s.find(']').unwrap() - 1;
        (s.replacen('[', "", 1).replacen(']', "", 1), a..b)
    }

    fn render(t: &str, sel: Range<usize>) -> String {
        if sel.is_empty() {
            format!("{}|{}", &t[..sel.start], &t[sel.start..])
        } else {
            format!("{}[{}]{}", &t[..sel.start], &t[sel.clone()], &t[sel.end..])
        }
    }

    #[test]
    fn comments_toggle_at_the_common_indent() {
        let c = |t: &str, s| toggle_comment(t, s, "--");
        check(c, "SEL|ECT 1", "-- SEL|ECT 1");
        check(c, "-- SEL|ECT 1", "SEL|ECT 1");
        check(c, "--SEL|ECT 1", "SEL|ECT 1");
        check(c, "  [a\n\n    b]\nc", "  [-- a\n\n  --   b]\nc");
        check(c, "[  -- a\n\n  --   b]", "[  a\n\n    b]");
        // A cursor toggles its own line only.
        check(c, "-- a\n-- b|", "-- a\nb|");
        // A partly commented block gets commented as a whole.
        check(c, "[-- a\nb]", "[-- -- a\n-- b]");
        // A selection ending at a line start leaves that line alone.
        check(c, "[a\n]b", "[-- a\n]b");
        check(|t: &str, s| toggle_comment(t, s, "//"), "db.c.find()|", "// db.c.find()|");
        check(c, "|", "-- |");
    }

    #[test]
    fn lines_duplicate_delete_and_move() {
        check(duplicate, "a\nb|c\nd", "a\nbc\nb|c\nd");
        check(duplicate, "a [b] c", "a b[b] c");
        check(delete_lines, "a\nb|c\nd", "a\n|d");
        check(delete_lines, "a\nb|c", "|a");
        check(delete_lines, "o|nly", "|");
        check(|t: &str, s| move_lines(t, s, true), "a\nb|c\nd", "b|c\na\nd");
        check(|t: &str, s| move_lines(t, s, false), "a\nb|c\nd", "a\nd\nb|c");
        check(|t: &str, s| move_lines(t, s, true), "a|\nb", "a|\nb");
        check(|t: &str, s| move_lines(t, s, false), "[a\nb]\nc", "c\n[a\nb]");
    }

    #[test]
    fn new_lines_keep_the_indent() {
        check(line_below, "  SEL|ECT 1\nx", "  SELECT 1\n  |\nx");
        check(line_above, "x\n    FR|OM t", "x\n    |\n    FROM t");
    }
}
