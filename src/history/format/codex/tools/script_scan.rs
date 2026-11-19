//! One pass over an `exec` script's JavaScript source that finds its tool
//! calls. It skips comments, string literals, template literals and regular
//! expression literals; a template literal's `${…}` is source and is
//! scanned.
//!
//! Each find carries a depth: the count of the brackets and brace-less
//! bodies around it that can run it more than once or not at all. Counted:
//! - a brace, except the one after `try` or `finally`;
//! - the parenthesis after `if`, `for`, `while`, `switch`, `catch` or
//!   `with`;
//! - an arrow function's body without braces, until `,` or `;`;
//! - the operand after `&&`, `||`, `??` or the `?` of a conditional, until
//!   `,` or `;`;
//! - the statement after `if (…)`, `else`, `for (…)`, `while (…)` or `do`
//!   without braces, until `;` or the end of its line.
//!
//! A call's own parentheses and an array's brackets don't count: their
//! contents run once.

use regex::Regex;
use std::sync::LazyLock;

#[derive(Default)]
pub(super) struct ScriptScan<'a> {
    /// The calls written `tools.<name>(` or `tools["<name>"](`, in source
    /// order.
    pub(super) calls: Vec<ScriptCall<'a>>,
    /// True when the source names `tools` other than in such a call, as in
    /// `tools[name](…)` or `const call = tools.write_stdin`: it may call
    /// tools the scan can't name.
    pub(super) names_tools_otherwise: bool,
}

pub(super) struct ScriptCall<'a> {
    pub(super) name: &'a str,
    pub(super) depth: usize,
    pub(super) script_before: &'a str,
    /// The script after the call's opening parenthesis.
    pub(super) rest_of_script: &'a str,
}

pub(super) fn scan(source: &str) -> ScriptScan<'_> {
    let mut scanner = Scanner {
        source,
        bytes: source.as_bytes(),
        position: 0,
        frames: vec![Frame::new(FrameKind::TopLevel, false)],
        previous: Previous::Nothing,
        awaits_condition: false,
        found: ScriptScan::default(),
    };
    scanner.run();
    scanner.found
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FrameKind {
    TopLevel,
    Paren,
    Bracket,
    Brace,
    /// A template literal's text.
    Template,
    /// A template literal's `${…}`.
    Substitution,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ControlBody {
    Closed,
    /// After `if (…)`, `else` or `do`, before the statement's first token.
    AwaitingStart,
    Open,
}

struct Frame {
    kind: FrameKind,
    /// A counted brace or condition parenthesis.
    is_counted: bool,
    /// The parenthesis after `if`, `for`, `while`, `switch`, `catch` or
    /// `with`; the statement after it is that keyword's body.
    is_condition: bool,
    in_arrow_body: bool,
    after_short_circuit: bool,
    control_body: ControlBody,
}

impl Frame {
    fn new(kind: FrameKind, is_counted: bool) -> Self {
        Self {
            kind,
            is_counted,
            is_condition: false,
            in_arrow_body: false,
            after_short_circuit: false,
            control_body: ControlBody::Closed,
        }
    }

    fn depth(&self) -> usize {
        usize::from(self.is_counted)
            + usize::from(self.in_arrow_body)
            + usize::from(self.after_short_circuit)
            + usize::from(self.control_body != ControlBody::Closed)
    }
}

/// The token before, as far as the next token's meaning depends on it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Previous<'a> {
    Nothing,
    Word(&'a str),
    /// `.` or `?.`: the next word is a property name.
    Dot,
    /// A literal, a property name, `)` or `]`: a `/` after it divides.
    Value,
    Operator,
}

const CONDITION_KEYWORDS: [&str; 6] = ["if", "for", "while", "switch", "catch", "with"];
/// Keywords after which an expression starts, so a `/` opens a regular
/// expression literal.
const EXPRESSION_KEYWORDS: [&str; 13] = [
    "return",
    "typeof",
    "instanceof",
    "in",
    "of",
    "new",
    "delete",
    "void",
    "throw",
    "case",
    "do",
    "else",
    "await",
];

static DOT_CALL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*\.\s*(\w+)\s*\(").unwrap());
static BRACKET_CALL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^\s*\[\s*(?:"(\w+)"|'(\w+)'|`(\w+)`)\s*\]\s*\("#).unwrap());

struct Scanner<'a> {
    source: &'a str,
    bytes: &'a [u8],
    position: usize,
    /// Never empty: the top level is the first frame.
    frames: Vec<Frame>,
    previous: Previous<'a>,
    /// After a condition keyword, until its parenthesis; `for await (` keeps
    /// it across `await`.
    awaits_condition: bool,
    found: ScriptScan<'a>,
}

impl<'a> Scanner<'a> {
    fn run(&mut self) {
        while self.position < self.bytes.len() {
            if self.top().kind == FrameKind::Template {
                self.scan_template_text();
            } else {
                self.scan_source_token();
            }
        }
    }

    fn top(&self) -> &Frame {
        self.frames.last().expect("the top level is never closed")
    }

    fn top_mut(&mut self) -> &mut Frame {
        self.frames
            .last_mut()
            .expect("the top level is never closed")
    }

    fn byte_at(&self, position: usize) -> Option<u8> {
        self.bytes.get(position).copied()
    }

    fn depth(&self) -> usize {
        self.frames.iter().map(Frame::depth).sum()
    }

    fn scan_template_text(&mut self) {
        match self.bytes[self.position] {
            b'\\' => self.position += 2,
            b'`' => {
                self.frames.pop();
                self.position += 1;
                self.previous = Previous::Value;
            }
            b'$' if self.byte_at(self.position + 1) == Some(b'{') => {
                self.frames.push(Frame::new(FrameKind::Substitution, false));
                self.position += 2;
                self.previous = Previous::Operator;
            }
            _ => self.position += 1,
        }
    }

    fn scan_source_token(&mut self) {
        let byte = self.bytes[self.position];
        if byte == b'\n' {
            if self.top().control_body == ControlBody::Open {
                self.top_mut().control_body = ControlBody::Closed;
            }
            self.position += 1;
            return;
        }
        if byte.is_ascii_whitespace() {
            self.position += 1;
            return;
        }
        if byte == b'/' && self.skip_comment() {
            return;
        }
        if byte == b'{' && self.top().control_body == ControlBody::AwaitingStart {
            // The brace is the statement's body, and counts itself.
            self.top_mut().control_body = ControlBody::Closed;
        } else if self.top().control_body == ControlBody::AwaitingStart {
            self.top_mut().control_body = ControlBody::Open;
        }
        let is_word = is_word_byte(byte);
        if self.awaits_condition && byte != b'(' && !is_word {
            self.awaits_condition = false;
        }
        match byte {
            _ if is_word => self.scan_word(),
            b'"' | b'\'' => self.skip_string(byte),
            b'`' => {
                self.frames.push(Frame::new(FrameKind::Template, false));
                self.position += 1;
            }
            b'/' if self.regular_expression_may_start() => self.skip_regular_expression(),
            b'(' => self.open_paren(),
            b'[' => self.open(FrameKind::Bracket, false),
            b'{' => {
                let is_counted = !matches!(self.previous, Previous::Word("try" | "finally"));
                self.open(FrameKind::Brace, is_counted);
            }
            b')' => self.close(FrameKind::Paren),
            b']' => self.close(FrameKind::Bracket),
            b'}' => self.close(FrameKind::Brace),
            b';' => {
                let frame = self.top_mut();
                frame.in_arrow_body = false;
                frame.after_short_circuit = false;
                frame.control_body = ControlBody::Closed;
                self.operator(1);
            }
            b',' => {
                let frame = self.top_mut();
                frame.in_arrow_body = false;
                frame.after_short_circuit = false;
                self.operator(1);
            }
            b'=' => self.scan_equals(),
            b'&' | b'|' if self.byte_at(self.position + 1) == Some(byte) => {
                self.top_mut().after_short_circuit = true;
                self.operator(2);
            }
            b'?' => self.scan_question_mark(),
            b'.' if self.source[self.position..].starts_with("...") => self.operator(3),
            b'.' if !self
                .byte_at(self.position + 1)
                .is_some_and(|next| next.is_ascii_digit()) =>
            {
                self.position += 1;
                self.previous = Previous::Dot;
            }
            _ => self.operator(1),
        }
    }

    fn operator(&mut self, length: usize) {
        self.position += length;
        self.previous = Previous::Operator;
    }

    /// Skip a comment at the position; false when the `/` opens none.
    fn skip_comment(&mut self) -> bool {
        let rest = &self.source[self.position..];
        if rest.starts_with("//") {
            self.position += rest.find('\n').unwrap_or(rest.len());
            true
        } else if let Some(comment) = rest.strip_prefix("/*") {
            self.position += comment.find("*/").map_or(rest.len(), |end| end + 4);
            true
        } else {
            false
        }
    }

    fn regular_expression_may_start(&self) -> bool {
        match self.previous {
            Previous::Word(word) => EXPRESSION_KEYWORDS.contains(&word),
            Previous::Value => false,
            Previous::Nothing | Previous::Dot | Previous::Operator => true,
        }
    }

    fn skip_string(&mut self, quote: u8) {
        let mut position = self.position + 1;
        while let Some(byte) = self.byte_at(position) {
            match byte {
                b'\\' => position += 2,
                // A string literal doesn't span lines; the source is broken.
                b'\n' => break,
                _ if byte == quote => {
                    position += 1;
                    break;
                }
                _ => position += 1,
            }
        }
        self.position = position.min(self.bytes.len());
        self.previous = Previous::Value;
    }

    fn skip_regular_expression(&mut self) {
        let mut position = self.position + 1;
        let mut in_class = false;
        while let Some(byte) = self.byte_at(position) {
            match byte {
                b'\\' => position += 2,
                b'\n' => break,
                b'[' => {
                    in_class = true;
                    position += 1;
                }
                b']' => {
                    in_class = false;
                    position += 1;
                }
                b'/' if !in_class => {
                    position += 1;
                    break;
                }
                _ => position += 1,
            }
        }
        self.position = position.min(self.bytes.len());
        self.previous = Previous::Value;
    }

    fn open(&mut self, kind: FrameKind, is_counted: bool) {
        self.frames.push(Frame::new(kind, is_counted));
        self.operator(1);
    }

    fn open_paren(&mut self) {
        let is_condition = self.awaits_condition;
        self.awaits_condition = false;
        self.open(FrameKind::Paren, is_condition);
        self.top_mut().is_condition = is_condition;
    }

    /// Close the innermost open `kind`, and any bracket left open inside it.
    /// A `}` also closes a template literal's `${`. A close without its
    /// opening bracket is skipped.
    fn close(&mut self, kind: FrameKind) {
        self.position += 1;
        self.previous = if kind == FrameKind::Brace {
            Previous::Operator
        } else {
            Previous::Value
        };
        for index in (1..self.frames.len()).rev() {
            let open = self.frames[index].kind;
            if open == kind || (kind == FrameKind::Brace && open == FrameKind::Substitution) {
                let is_condition = self.frames[index].is_condition;
                self.frames.truncate(index);
                if is_condition {
                    self.top_mut().control_body = ControlBody::AwaitingStart;
                }
                return;
            }
            if matches!(open, FrameKind::Template | FrameKind::Substitution) {
                return;
            }
        }
    }

    fn scan_equals(&mut self) {
        let rest = &self.source[self.position..];
        if rest.starts_with("=>") {
            self.operator(2);
            let body = self.source[self.position..].trim_start();
            if !body.starts_with('{') {
                self.top_mut().in_arrow_body = true;
            }
        } else if rest.starts_with("===") {
            self.operator(3);
        } else if rest.starts_with("==") {
            self.operator(2);
        } else {
            self.operator(1);
        }
    }

    fn scan_question_mark(&mut self) {
        let next = self.byte_at(self.position + 1);
        if next == Some(b'.')
            && !self
                .byte_at(self.position + 2)
                .is_some_and(|byte| byte.is_ascii_digit())
        {
            self.position += 2;
            self.previous = Previous::Dot;
            return;
        }
        self.top_mut().after_short_circuit = true;
        self.operator(if next == Some(b'?') { 2 } else { 1 });
    }

    fn scan_word(&mut self) {
        let start = self.position;
        let end = self.bytes[start..]
            .iter()
            .position(|byte| !is_word_byte(*byte))
            .map_or(self.bytes.len(), |length| start + length);
        let word = &self.source[start..end];
        self.position = end;
        if word.as_bytes()[0].is_ascii_digit() {
            self.previous = Previous::Value;
            return;
        }
        if self.previous == Previous::Dot {
            self.previous = Previous::Value;
            return;
        }
        if !(word == "await" && self.awaits_condition) {
            self.awaits_condition = false;
        }
        self.previous = Previous::Word(word);
        if word == "tools" {
            self.scan_tools_use(start);
        } else if CONDITION_KEYWORDS.contains(&word) {
            self.awaits_condition = true;
        } else if matches!(word, "else" | "do") {
            self.top_mut().control_body = ControlBody::AwaitingStart;
        }
    }

    /// Record a call at `tools`, and move to its opening parenthesis.
    fn scan_tools_use(&mut self, start: usize) {
        let rest = &self.source[self.position..];
        let call = DOT_CALL
            .captures(rest)
            .or_else(|| BRACKET_CALL.captures(rest));
        let Some(call) = call else {
            self.found.names_tools_otherwise = true;
            return;
        };
        let name = call
            .iter()
            .skip(1)
            .flatten()
            .next()
            .map_or("", |name| name.as_str());
        let after_parenthesis = call.get_match().end();
        self.found.calls.push(ScriptCall {
            name,
            depth: self.depth(),
            script_before: &self.source[..start],
            rest_of_script: &rest[after_parenthesis..],
        });
        self.position += after_parenthesis - 1;
        self.previous = Previous::Value;
    }
}

/// A byte of a name or a number. Bytes outside ASCII count, so a name never
/// ends inside a character.
fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$') || !byte.is_ascii()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn calls(source: &str) -> Vec<(&str, usize)> {
        scan(source)
            .calls
            .iter()
            .map(|call| (call.name, call.depth))
            .collect()
    }

    #[test]
    fn calls_in_comments_and_literals_arent_found() {
        let source = "// await tools.update_plan({})\n/* tools.a() */ const s = \"tools.b()\" + 'tools.c(' + `tools.d(${await tools.e({})})`;\nconst r = /tools.f(/.test(s) ? 1 : 2;\ntext(await tools.g({}));";
        assert_eq!(calls(source), [("e", 0), ("g", 0)]);
    }

    #[test]
    fn call_arguments_and_array_brackets_dont_count() {
        assert_eq!(
            calls("text(JSON.stringify(await Promise.all([tools.a({ x: [1] })])));"),
            [("a", 0)]
        );
    }

    #[test]
    fn loops_callbacks_branches_and_functions_count() {
        for source in [
            "for (const id of ids) { await tools.a({}); }",
            "for (const id of ids) await tools.a({});",
            "while (await tools.a({})) {}",
            "if (ready)\n  await tools.a({});",
            "if (ready) {} else await tools.a({});",
            "do await tools.a({}); while (again)",
            "ids.map(id => tools.a({ id }));",
            "const f = () => tools.a({});",
            "async function f() { await tools.a({}); }",
            "ready && await tools.a({});",
            "const r = ready ? await tools.a({}) : null;",
            "const r = cached ?? await tools.a({});",
            "try {} catch (error) { await tools.a({}); }",
        ] {
            assert_eq!(
                calls(source),
                [("a", 1)],
                "{source} runs `a` other than once"
            );
        }
    }

    #[test]
    fn statement_after_a_braceless_body_or_try_block_is_top_level() {
        for source in [
            "if (!ready) text(\"waiting\")\nawait tools.a({});",
            "if (!ready) { text(\"waiting\"); }\nawait tools.a({});",
            "ids.forEach(id => text(id));\nawait tools.a({});",
            "const x = a && b;\nawait tools.a({});",
            "try { await tools.a({}); } finally { text(\"done\"); }",
            "const r = await tools.a({}) ?? fallback;",
        ] {
            assert_eq!(calls(source), [("a", 0)], "{source} runs `a` once");
        }
    }

    #[test]
    fn bracketed_name_is_call_and_any_other_use_of_tools_is_noted() {
        let named = scan("await tools[\"write_stdin\"]({ session_id: 1 });");
        assert_eq!(named.calls.len(), 1);
        assert_eq!(named.calls[0].name, "write_stdin");
        assert!(!named.names_tools_otherwise);

        for source in [
            "await tools[name]({});",
            "const call = tools.write_stdin; await call({});",
        ] {
            assert!(
                scan(source).names_tools_otherwise,
                "{source} may call tools the scan can't name"
            );
        }
        for source in [
            "itertools.zip_longest(a, b)",
            "x.tools.a()",
            "ALL_TOOLS.filter(f)",
        ] {
            let found = scan(source);
            assert!(
                found.calls.is_empty() && !found.names_tools_otherwise,
                "{source} doesn't name the `tools` global"
            );
        }
    }
}
