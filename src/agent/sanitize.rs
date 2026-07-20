pub fn sanitize_agent_text(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            consume_escape_sequence(&mut chars);
            continue;
        }
        if ch == '\n' || ch == '\t' || (!ch.is_control() && ch != '\u{7f}') {
            output.push(ch);
        }
    }
    output
}

fn consume_escape_sequence(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    let Some(first) = chars.next() else {
        return;
    };
    match first {
        '[' => {
            for ch in chars.by_ref() {
                if ('@'..='~').contains(&ch) {
                    break;
                }
            }
        }
        ']' | 'P' | '^' | '_' => {
            let mut previous_escape = false;
            for ch in chars.by_ref() {
                if ch == '\u{7}' || (previous_escape && ch == '\\') {
                    break;
                }
                previous_escape = ch == '\u{1b}';
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_ansi_and_terminal_control_sequences() {
        let input = "safe\u{1b}[31mred\u{1b}[0m\u{1b}]0;title\u{7}text\u{1b}Ppayload\u{1b}\\done";

        assert_eq!(sanitize_agent_text(input), "saferedtextdone");
    }

    #[test]
    fn preserves_unicode_line_breaks_and_tabs() {
        assert_eq!(sanitize_agent_text("α\tβ\nγ\u{0}δ\u{7f}"), "α\tβ\nγδ");
    }
}
