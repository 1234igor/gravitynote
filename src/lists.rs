//! Editing the structure of a line: its indentation, and whether it is a task.
//!
//! Every function here takes one line and returns what that line should become.
//! The view applies them across whatever lines the selection touches, so the
//! same rule serves one line and twenty.

use crate::markdown;

/// One level of indentation. Two spaces is what a nested markdown list wants,
/// and it survives being read in any other editor — a tab does not.
pub const INDENT: &str = "  ";

/// The leading whitespace of a line.
pub fn indent_of(line: &str) -> &str {
    let end = line.len() - line.trim_start_matches([' ', '\t']).len();
    &line[..end]
}

/// The line moved one level in.
///
/// Blank lines included: pressing Tab on the empty line you just made is how
/// you start something nested, and refusing would be refusing the main use.
pub fn indent(line: &str) -> String {
    format!("{INDENT}{line}")
}

/// The same, for a line inside a multi-line selection.
///
/// Here a blank line is a gap between items rather than somewhere you are about
/// to type, and indenting it would leave trailing whitespace nobody asked for
/// or can see.
pub fn indent_in_block(line: &str) -> String {
    if line.trim().is_empty() {
        return line.to_string();
    }
    indent(line)
}

/// The line moved one level out, if it has anywhere to go.
///
/// Takes a tab as a whole level, so a line indented by hand in another editor
/// outdents in one press rather than two.
pub fn outdent(line: &str) -> String {
    if let Some(rest) = line.strip_prefix(INDENT) {
        return rest.to_string();
    }
    if let Some(rest) = line.strip_prefix('\t') {
        return rest.to_string();
    }
    // Less than a full level: take what there is.
    line.trim_start_matches(' ').to_string()
}

/// Whether the line can move one level out.
pub fn can_outdent(line: &str) -> bool {
    !indent_of(line).is_empty()
}

/// The line as a task, or back to an ordinary list item if it already is one.
///
/// Only the box goes on and off. The bullet is what makes the line a list item
/// and it is kept either way — including the number of an ordered one, which
/// there would be no way to recover. Plain text gains a bullet along with its
/// box, because that is what it has become.
pub fn toggle_task(line: &str) -> String {
    // A rule is not a line to make a task of. Turning it into `- [ ] ---`
    // would stop it dividing the notes, silently merging them — and the rule
    // is drawn invisible, so a selection crossing one gives no warning.
    if markdown::is_separator(line) {
        return line.to_string();
    }
    let indent = indent_of(line).to_string();
    let body = &line[indent.len()..];

    if let Some((bullet, rest)) = split_marker(body) {
        if let Some(after_box) = strip_task_box(rest) {
            return format!("{indent}{bullet}{after_box}");
        }
        return format!("{indent}{bullet}[ ] {rest}");
    }
    if body.trim().is_empty() {
        return format!("{indent}- [ ] ");
    }
    format!("{indent}- [ ] {body}")
}

/// The line's box ticked, or unticked if it is already. Lines that are not
/// tasks are left alone: completing something that was never a task is a
/// question with no good answer.
pub fn toggle_done(line: &str) -> String {
    let indent = indent_of(line);
    let body = &line[indent.len()..];
    let Some((bullet, rest)) = split_marker(body) else {
        return line.to_string();
    };
    let Some(after_box) = strip_task_box(rest) else {
        return line.to_string();
    };
    let ticked = rest.as_bytes().get(1).is_some_and(|b| *b != b' ');
    let box_now = if ticked { "[ ] " } else { "[x] " };
    format!("{indent}{bullet}{box_now}{after_box}")
}

/// Whether the line carries a ticked box — what the menu shows a mark against.
pub fn is_done(line: &str) -> bool {
    let body = &line[indent_of(line).len()..];
    let Some((_, rest)) = split_marker(body) else {
        return false;
    };
    strip_task_box(rest).is_some() && rest.as_bytes().get(1).is_some_and(|b| *b != b' ')
}

/// Split a bullet (`- `, `* `, `1. `) off the head of an unindented body.
fn split_marker(body: &str) -> Option<(&str, &str)> {
    let end = markdown::list_marker_end(body)?;
    Some((&body[..end], &body[end..]))
}

/// The text after a `[ ]` / `[x]` box, if one starts here.
fn strip_task_box(rest: &str) -> Option<&str> {
    let b = rest.as_bytes();
    if b.len() < 3 || b[0] != b'[' || b[2] != b']' {
        return None;
    }
    if !matches!(b[1], b' ' | b'x' | b'X') {
        return None;
    }
    Some(rest[3..].strip_prefix(' ').unwrap_or(&rest[3..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indenting_moves_a_line_in_and_out() {
        assert_eq!(indent("- milk"), "  - milk");
        assert_eq!(outdent("  - milk"), "- milk");
        assert_eq!(indent("plain text"), "  plain text");
        assert_eq!(outdent("  plain text"), "plain text");
        // Round trip.
        assert_eq!(outdent(&indent("- milk")), "- milk");
    }

    #[test]
    fn outdenting_stops_at_the_margin() {
        assert_eq!(outdent("- milk"), "- milk");
        assert!(!can_outdent("- milk"));
        assert!(can_outdent("  - milk"));
        assert!(can_outdent("\t- milk"));
        // A hand-made tab is one whole level.
        assert_eq!(outdent("\t- milk"), "- milk");
        // A partial level goes to the margin rather than sticking.
        assert_eq!(outdent(" - milk"), "- milk");
    }

    /// Tab on the empty line you just made is how a nested item starts.
    #[test]
    fn the_empty_line_you_are_on_does_indent() {
        assert_eq!(indent(""), INDENT);
        assert_eq!(indent("  "), "    ");
    }

    /// Inside a block, a blank line is a gap — indenting it would leave
    /// trailing whitespace that cannot be seen and did not need to exist.
    #[test]
    fn a_blank_line_inside_a_block_is_left_alone() {
        assert_eq!(indent_in_block(""), "");
        assert_eq!(indent_in_block("   "), "   ");
        assert_eq!(indent_in_block("- milk"), "  - milk");
    }

    #[test]
    fn making_a_task_keeps_the_indent_and_the_bullet() {
        assert_eq!(toggle_task("buy milk"), "- [ ] buy milk");
        assert_eq!(toggle_task("- buy milk"), "- [ ] buy milk");
        assert_eq!(toggle_task("  - buy milk"), "  - [ ] buy milk");
        assert_eq!(toggle_task("* buy milk"), "* [ ] buy milk");
        assert_eq!(toggle_task("1. buy milk"), "1. [ ] buy milk");
        assert_eq!(toggle_task("    plain"), "    - [ ] plain");
    }

    #[test]
    fn un_making_a_task_keeps_the_line_a_list_item() {
        assert_eq!(toggle_task("- [ ] buy milk"), "- buy milk");
        assert_eq!(toggle_task("- [x] buy milk"), "- buy milk");
        assert_eq!(toggle_task("  - [ ] buy milk"), "  - buy milk");
        // The number of an ordered item could not be recovered, so it stays.
        assert_eq!(toggle_task("1. [ ] buy milk"), "1. buy milk");
    }

    #[test]
    fn a_list_item_survives_a_round_trip() {
        for line in ["- buy milk", "  - buy milk", "1. buy milk", "* buy milk"] {
            let task = toggle_task(line);
            assert!(task.contains("[ ]"), "{line:?} did not become a task");
            assert_eq!(toggle_task(&task), line, "round trip of {line:?}");
        }
        // Plain text keeps the bullet it gained: it is a list item now.
        assert_eq!(toggle_task(&toggle_task("buy milk")), "- buy milk");
    }

    #[test]
    fn completing_ticks_and_unticks_the_box() {
        assert_eq!(toggle_done("- [ ] buy milk"), "- [x] buy milk");
        assert_eq!(toggle_done("- [x] buy milk"), "- [ ] buy milk");
        assert_eq!(toggle_done("  - [X] buy milk"), "  - [ ] buy milk");
        assert_eq!(toggle_done("1. [ ] numbered"), "1. [x] numbered");
        assert!(is_done("- [x] buy milk"));
        assert!(!is_done("- [ ] buy milk"));
    }

    /// Selecting two notes and pressing ⌘⇧T must not turn the rule between
    /// them into a task — that merges the notes and leaves a `- [ ] ---` line
    /// nobody typed.
    #[test]
    fn a_rule_is_never_made_into_a_task() {
        for rule in ["---", "***", "___", "- - -", "  ---  "] {
            assert_eq!(toggle_task(rule), rule, "rule {rule:?}");
            assert_eq!(toggle_done(rule), rule, "rule {rule:?}");
        }
    }

    #[test]
    fn completing_leaves_anything_that_is_not_a_task_alone() {
        for line in ["buy milk", "- buy milk", "", "   ", "# heading", "---"] {
            assert_eq!(toggle_done(line), line, "line {line:?}");
            assert!(!is_done(line));
        }
    }

    #[test]
    fn an_empty_line_becomes_an_empty_task() {
        assert_eq!(toggle_task(""), "- [ ] ");
        assert_eq!(toggle_task("  "), "  - [ ] ");
    }
}
