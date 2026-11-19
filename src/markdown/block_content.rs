//! The Markdown inside a block quote or a list item, without the `>` marks,
//! bullet or number that contain it.

/// Columns from one tab stop to the next, as CommonMark counts a tab.
const TAB_STOP: usize = 4;

/// `quote`, a block quote's Markdown, without one `>` mark, and the space or
/// tab after it, on each line. A lazy line without a mark stays as it is.
pub(crate) fn quote_content(quote: &str) -> String {
    quote
        .lines()
        .map(|line| {
            let unindented = line.trim_start_matches(' ');
            match unindented.strip_prefix('>') {
                Some(rest) => rest
                    .strip_prefix(' ')
                    .or_else(|| rest.strip_prefix('\t'))
                    .unwrap_or(rest),
                None => line,
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `item`, a list item's Markdown, without its bullet or number, and each
/// later line without the indent that puts it inside the item. Empty for an
/// item without content.
pub(crate) fn list_item_content(item: &str) -> String {
    let mut lines = item.lines();
    let first = lines.next().unwrap_or_default();
    let marker = ItemMarker::of(first);
    let first_content = unindented(
        &first[marker.end..],
        marker.end_column,
        marker.content_column,
    );
    let later = lines.map(|line| unindented(line, 0, marker.content_column));
    let content: Vec<String> = if first_content.is_empty() {
        later.collect()
    } else {
        std::iter::once(first_content).chain(later).collect()
    };
    content.join("\n")
}

/// A list item's bullet or number on its first line.
struct ItemMarker {
    /// The byte after the marker.
    end: usize,
    /// The column after the marker.
    end_column: usize,
    /// The column the item's content starts at.
    content_column: usize,
}

impl ItemMarker {
    /// The marker of the item whose first line is `first_line`. The content
    /// starts one column after the marker when the rest of the line is blank,
    /// or when five columns or more of whitespace follow the marker, because
    /// those open an indented code block; otherwise after the whitespace.
    fn of(first_line: &str) -> Self {
        let indent_end = first_line.len() - first_line.trim_start_matches([' ', '\t']).len();
        let indent_columns = columns_after(&first_line[..indent_end], 0);
        let after_indent = &first_line[indent_end..];
        let digits = after_indent.len()
            - after_indent
                .trim_start_matches(|c: char| c.is_ascii_digit())
                .len();
        let marker_len = if digits > 0 {
            digits + 1
        } else {
            after_indent.chars().next().map_or(0, char::len_utf8)
        };
        let end = (indent_end + marker_len).min(first_line.len());
        let end_column = indent_columns + marker_len;
        let after_marker = &first_line[end..];
        let whitespace_end =
            after_marker.len() - after_marker.trim_start_matches([' ', '\t']).len();
        let whitespace_columns =
            columns_after(&after_marker[..whitespace_end], end_column) - end_column;
        let is_blank = whitespace_end == after_marker.len();
        let content_offset = if !is_blank && (1..=TAB_STOP).contains(&whitespace_columns) {
            whitespace_columns
        } else {
            1
        };
        Self {
            end,
            end_column,
            content_column: end_column + content_offset,
        }
    }
}

/// The column after `whitespace`, which starts at `column`.
fn columns_after(whitespace: &str, column: usize) -> usize {
    whitespace.chars().fold(column, next_column)
}

/// The column after `character` at `column`: a tab reaches the next tab
/// stop.
fn next_column(column: usize, character: char) -> usize {
    match character {
        '\t' => (column / TAB_STOP + 1) * TAB_STOP,
        _ => column + 1,
    }
}

/// `line`, which starts at `column`, without the spaces and tabs before
/// `content_column`. A tab that crosses `content_column` leaves a space for
/// each column of it past `content_column`.
fn unindented(line: &str, mut column: usize, content_column: usize) -> String {
    for (index, character) in line.char_indices() {
        if column >= content_column || !matches!(character, ' ' | '\t') {
            return line[index..].to_owned();
        }
        let next = next_column(column, character);
        if next > content_column {
            return " ".repeat(next - content_column) + &line[index + 1..];
        }
        column = next;
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_content_drops_one_mark_per_line() {
        assert_eq!(
            quote_content("> **Note:** one\nlazy line\n> ```\n> code\n> ```"),
            "**Note:** one\nlazy line\n```\ncode\n```"
        );
        assert_eq!(
            quote_content("> outer\n> > inner"),
            "outer\n> inner",
            "a nested quote keeps its own marks"
        );
        assert_eq!(
            quote_content(">\tfoo"),
            "foo",
            "a tab after the mark is removed"
        );
    }

    #[test]
    fn list_item_content_drops_marker_and_content_indent() {
        assert_eq!(
            list_item_content("1. Build:\n\n   ```sh\n   cargo build\n   ```"),
            "Build:\n\n```sh\ncargo build\n```",
            "a code block inside an item keeps its fences"
        );
        assert_eq!(
            list_item_content("2. Check\n   - fmt\n   - clippy"),
            "Check\n- fmt\n- clippy",
            "a nested list stays in its item"
        );
        assert_eq!(list_item_content("-   wide\n    gap"), "wide\ngap");
    }

    #[test]
    fn list_item_content_starting_on_next_line_is_unindented() {
        assert_eq!(list_item_content("-\n  foo"), "foo");
        assert_eq!(list_item_content("1.\n   foo\n   bar"), "foo\nbar");
    }

    #[test]
    fn list_item_content_drops_tab_after_marker() {
        assert_eq!(list_item_content("-\tfoo"), "foo");
        assert_eq!(list_item_content("-\tfoo\n\tbar"), "foo\nbar");
    }

    #[test]
    fn empty_list_item_has_empty_content() {
        assert_eq!(list_item_content("-"), "");
        assert_eq!(list_item_content("1."), "");
    }

    #[test]
    fn five_spaces_after_marker_keep_code_indent() {
        assert_eq!(list_item_content("-     code"), "    code");
    }
}
