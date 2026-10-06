//! Shared code reaches an agent only through its provider: each agent's own
//! code lives in its files under `src/history/provider/` and
//! `src/history/format/` (`claude.rs`, or a `claude/` directory). Code in
//! any other file that names an agent's `Source` variant, or a path into an
//! agent's module, fails this test. The registry statics that map a
//! `Source` to its provider are the one exception. Comments, strings and
//! test code may name agents.
//!
//! The test does not catch an agent named in a string (an environment
//! variable, a CLI name), an identifier holding an agent's name
//! (`ClaudeProvider` outside a path), or behavior specific to one agent that
//! names none, such as matching sessions by their parent folder's name.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// The directories holding each agent's own files.
const AGENT_DIRECTORIES: [&str; 2] = ["src/history/provider", "src/history/format"];
/// The file holding the registry statics, `static CLAUDE: claude::ClaudeProvider`.
const REGISTRY_FILE: &str = "src/history/provider/mod.rs";
const SOURCE_VARIANTS: [&str; 6] = ["Claude", "Codex", "OpenCode", "Kimi", "Pi", "Omp"];
const AGENT_MODULES: [&str; 7] = ["claude", "codex", "opencode", "kimi", "pi", "omp", "pi_log"];
const AGENT_MODULE_PARENTS: [&str; 2] = ["provider", "format"];

#[test]
fn shared_code_names_no_agent() {
    let crate_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let files = rust_files(&crate_root.join("src"));
    let test_only = test_only_files(&files);
    let agent_files: Vec<PathBuf> = AGENT_DIRECTORIES
        .iter()
        .flat_map(|directory| {
            AGENT_MODULES.iter().flat_map(move |module| {
                let module = crate_root.join(directory).join(module);
                [module.with_extension("rs"), module]
            })
        })
        .collect();
    let registry = crate_root.join(REGISTRY_FILE);

    let mut violations = Vec::new();
    for file in &files {
        if test_only.contains(file) || agent_files.iter().any(|agent| file.starts_with(agent)) {
            continue;
        }
        let source = std::fs::read_to_string(file).unwrap();
        let registry_statics = if *file == registry {
            static_item_lines(&source)
        } else {
            HashSet::new()
        };
        for (line, name) in agent_names_in(&source) {
            if registry_statics.contains(&line) {
                continue;
            }
            let relative = file.strip_prefix(crate_root).unwrap_or(file);
            violations.push(format!("{}:{line}: {name}", relative.display()));
        }
    }

    assert!(
        violations.is_empty(),
        "shared code names an agent; reach it through its provider instead:\n{}",
        violations.join("\n")
    );
}

#[test]
fn an_agent_named_in_shared_code_is_reported_by_line() {
    let source = r##"
        // Source::Claude in a comment
        fn shared() { let _ = "Source::Codex"; let source = Source::Kimi; }
        use crate::history::provider::claude::subagent_transcripts;
        use crate::history::format::{codex, splice};
        fn lifetime<'a>(text: &'a str) -> char { let _ = text; '}' }
        fn not_an_agent() { let _ = Source::KimiLike; my_provider::claude(); }
        #[cfg(test)]
        fn helper() -> Source { Source::Pi }
        #[cfg(test)]
        fn with_parameters(
            name: &str,
        ) -> Source { Source::Codex }
        struct Counted {
            #[cfg(test)]
            count: usize,
            source: Source,
        }
        #[cfg(test)]
        mod tests {
            fn quoted() { let _ = r#"{"type":"}"#; Source::Omp; }
        }
        fn after_the_tests() { Source::OpenCode; }
        use crate::history::format::{
            kimi::wire_location,
            splice,
        };
        fn relative() { super::pi::session_root(); omp::OMP_LOG; }
    "##;

    assert_eq!(
        agent_names_in(source),
        vec![
            (3, "Source::Kimi".to_owned()),
            (4, "provider::claude".to_owned()),
            (5, "format::codex".to_owned()),
            (23, "Source::OpenCode".to_owned()),
            (24, "format::kimi".to_owned()),
            (28, "pi::".to_owned()),
            (28, "omp::".to_owned()),
        ]
    );
}

/// The registry statics are exempt, and nothing else in their file is.
#[test]
fn only_static_items_are_exempt_in_the_registry() {
    let source = "
        static CLAUDE: claude::ClaudeProvider = claude::ClaudeProvider;
        fn provider() -> &'static dyn SessionProvider { &codex::CodexProvider }
    ";

    assert_eq!(static_item_lines(source), HashSet::from([2]));
    assert_eq!(
        agent_names_in(source)
            .into_iter()
            .filter(|(line, _)| !static_item_lines(source).contains(line))
            .collect::<Vec<_>>(),
        vec![(3, "codex::".to_owned())]
    );
}

/// The line number and text of every agent name in `source`'s code. A `use`
/// statement wrapped over several lines is reported at its first line.
fn agent_names_in(source: &str) -> Vec<(usize, String)> {
    let code = without_test_items(&code_only(source));
    let mut names = Vec::new();
    for (line_number, statement) in statements_by_line(&code) {
        for variant in SOURCE_VARIANTS {
            let name = format!("Source::{variant}");
            if contains_word(&statement, &name) {
                names.push((line_number, name));
            }
        }
        let mut named_under_a_parent = HashSet::new();
        for parent in AGENT_MODULE_PARENTS {
            for module in modules_under(&statement, parent) {
                if AGENT_MODULES.contains(&module.as_str()) {
                    names.push((line_number, format!("{parent}::{module}")));
                    named_under_a_parent.insert(module);
                }
            }
        }
        for module in AGENT_MODULES {
            if !named_under_a_parent.contains(module) && names_module_relatively(&statement, module)
            {
                names.push((line_number, format!("{module}::")));
            }
        }
    }
    names
}

/// Each line of `code` with its 1-based number, except that a `use`
/// statement spanning several lines is joined into one, numbered by its
/// first line.
fn statements_by_line(code: &str) -> Vec<(usize, String)> {
    let mut statements = Vec::new();
    let mut lines = code.lines().enumerate();
    while let Some((index, line)) = lines.next() {
        let mut statement = line.to_owned();
        if is_use_statement(line) {
            while !statement.contains(';') {
                let Some((_, next)) = lines.next() else {
                    break;
                };
                statement.push(' ');
                statement.push_str(next);
            }
        }
        statements.push((index + 1, statement));
    }
    statements
}

fn is_use_statement(line: &str) -> bool {
    let line = line.trim_start();
    let line = line
        .strip_prefix("pub(crate) ")
        .or_else(|| line.strip_prefix("pub(super) "))
        .or_else(|| line.strip_prefix("pub "))
        .unwrap_or(line);
    line.starts_with("use ")
}

/// True when `statement` names `module::` other than right after a
/// `provider::` or `format::` parent: `super::claude::`, or `claude::`
/// alone.
fn names_module_relatively(statement: &str, module: &str) -> bool {
    let path = format!("{module}::");
    statement.match_indices(&path).any(|(start, _)| {
        let before = &statement[..start];
        !before.chars().next_back().is_some_and(is_identifier_char)
            && !AGENT_MODULE_PARENTS
                .iter()
                .any(|parent| before.ends_with(&format!("{parent}::")))
    })
}

/// The line numbers of `source`'s `static` items.
fn static_item_lines(source: &str) -> HashSet<usize> {
    source
        .lines()
        .enumerate()
        .filter(|(_, line)| line.trim_start().starts_with("static "))
        .map(|(index, _)| index + 1)
        .collect()
}

fn is_identifier_char(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

/// True when `word` occurs in `line` with no identifier character on either
/// side.
fn contains_word(line: &str, word: &str) -> bool {
    line.match_indices(word).any(|(start, _)| {
        let before = line[..start].chars().next_back();
        let after = line[start + word.len()..].chars().next();
        !before.is_some_and(is_identifier_char) && !after.is_some_and(is_identifier_char)
    })
}

/// The modules `line` names under `parent::`: the one after it, or each one
/// in a `{…}` group after it.
fn modules_under(line: &str, parent: &str) -> Vec<String> {
    let prefix = format!("{parent}::");
    let mut modules = Vec::new();
    for (start, _) in line.match_indices(&prefix) {
        if line[..start]
            .chars()
            .next_back()
            .is_some_and(is_identifier_char)
        {
            continue;
        }
        let rest = &line[start + prefix.len()..];
        let names = match rest.strip_prefix('{') {
            Some(group) => group.split('}').next().unwrap_or_default(),
            None => rest,
        };
        for name in names.split(',') {
            let module: String = name
                .trim_start()
                .chars()
                .take_while(|character| is_identifier_char(*character))
                .collect();
            if !module.is_empty() {
                modules.push(module);
            }
            if !rest.starts_with('{') {
                break;
            }
        }
    }
    modules
}

/// `source` with comments removed and string and character literals emptied,
/// its lines kept where they were.
fn code_only(source: &str) -> String {
    let chars: Vec<char> = source.chars().collect();
    let mut code = String::with_capacity(source.len());
    let mut index = 0;
    while index < chars.len() {
        let character = chars[index];
        let next = chars.get(index + 1).copied();
        if character == '/' && next == Some('/') {
            while index < chars.len() && chars[index] != '\n' {
                index += 1;
            }
        } else if character == '/' && next == Some('*') {
            let mut depth = 0;
            while index < chars.len() {
                if chars[index] == '/' && chars.get(index + 1) == Some(&'*') {
                    depth += 1;
                    index += 2;
                } else if chars[index] == '*' && chars.get(index + 1) == Some(&'/') {
                    depth -= 1;
                    index += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    if chars[index] == '\n' {
                        code.push('\n');
                    }
                    index += 1;
                }
            }
        } else if let Some(hashes) = raw_string_hashes(&chars, index) {
            index = skip_raw_string(&chars, index, hashes, &mut code);
        } else if character == '"' {
            index += 1;
            while index < chars.len() && chars[index] != '"' {
                if chars[index] == '\\' {
                    index += 1;
                }
                if chars.get(index) == Some(&'\n') {
                    code.push('\n');
                }
                index += 1;
            }
            index += 1;
            code.push_str("\"\"");
        } else if character == '\'' && (next == Some('\\') || chars.get(index + 2) == Some(&'\'')) {
            index += 2;
            while index < chars.len() && chars[index] != '\'' {
                index += 1;
            }
            index += 1;
            code.push_str("' '");
        } else {
            code.push(character);
            index += 1;
        }
    }
    code
}

/// The `#` count of a raw string literal opening at `index` (`r"`, `r#"`,
/// `br"`), or `None` when none opens there.
fn raw_string_hashes(chars: &[char], index: usize) -> Option<usize> {
    let mut position = index;
    if chars.get(position) == Some(&'b') {
        position += 1;
    }
    if chars.get(position) != Some(&'r') || index > 0 && is_identifier_char(chars[index - 1]) {
        return None;
    }
    position += 1;
    let mut hashes = 0;
    while chars.get(position) == Some(&'#') {
        hashes += 1;
        position += 1;
    }
    (chars.get(position) == Some(&'"')).then_some(hashes)
}

/// The index just past the raw string opening at `index`, its lines kept in
/// `code`.
fn skip_raw_string(chars: &[char], index: usize, hashes: usize, code: &mut String) -> usize {
    let mut position = index;
    while chars[position] != '"' {
        position += 1;
    }
    position += 1;
    while position < chars.len() {
        if chars[position] == '"'
            && (1..=hashes).all(|offset| chars.get(position + offset) == Some(&'#'))
        {
            code.push_str("\"\"");
            return position + 1 + hashes;
        }
        if chars[position] == '\n' {
            code.push('\n');
        }
        position += 1;
    }
    position
}

/// `code` with every `#[cfg(test)]` item blanked, its lines kept where they
/// were. `code` holds no comments or literals, so its braces balance.
fn without_test_items(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut kept = Vec::with_capacity(lines.len());
    let mut index = 0;
    while index < lines.len() {
        let Some(rest) = lines[index].trim_start().strip_prefix("#[cfg(test)]") else {
            kept.push(lines[index]);
            index += 1;
            continue;
        };
        let mut item_line = rest;
        let mut depth = 0;
        let mut opened = false;
        let mut open_brackets = 0;
        loop {
            kept.push("");
            if !item_line.trim_start().starts_with("#[") {
                for character in item_line.chars() {
                    match character {
                        '{' => {
                            depth += 1;
                            opened = true;
                        }
                        '}' => depth -= 1,
                        '(' | '[' => open_brackets += 1,
                        ')' | ']' => open_brackets -= 1,
                        _ => {}
                    }
                }
                // A field or a statement ends at its separator, outside any
                // parameter list.
                let item_ended = if opened {
                    depth <= 0
                } else {
                    open_brackets == 0 && item_line.trim_end().ends_with([';', ','])
                };
                if item_ended {
                    break;
                }
            }
            index += 1;
            let Some(line) = lines.get(index) else {
                break;
            };
            item_line = line;
        }
        index += 1;
    }
    kept.join("\n")
}

fn rust_files(directory: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(rust_files(&path));
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
    files
}

/// The files of every module declared `#[cfg(test)] mod name;`, and every
/// file beneath one.
fn test_only_files(files: &[PathBuf]) -> HashSet<PathBuf> {
    let mut module_paths = Vec::new();
    for file in files {
        let source = code_only(&std::fs::read_to_string(file).unwrap());
        let lines: Vec<&str> = source.lines().map(str::trim).collect();
        for (index, line) in lines.iter().enumerate() {
            if *line != "#[cfg(test)]" {
                continue;
            }
            let Some(name) = lines.get(index + 1).and_then(|item| declared_module(item)) else {
                continue;
            };
            module_paths.push(child_module_directory(file).join(name));
        }
    }
    files
        .iter()
        .filter(|file| {
            module_paths
                .iter()
                .any(|module| file.starts_with(module) || **file == module.with_extension("rs"))
        })
        .cloned()
        .collect()
}

/// `name` in `mod name;` or `pub(crate) mod name;`.
fn declared_module(item: &str) -> Option<&str> {
    let declaration = item.strip_suffix(';')?;
    let name = declaration.rsplit_once("mod ")?.1.trim();
    name.chars().all(is_identifier_char).then_some(name)
}

/// The directory holding the files of modules `file` declares.
fn child_module_directory(file: &Path) -> PathBuf {
    let parent = file.parent().unwrap();
    match file.file_name().and_then(|name| name.to_str()) {
        Some("main.rs" | "lib.rs" | "mod.rs") => parent.to_path_buf(),
        _ => parent.join(file.file_stem().unwrap()),
    }
}
