//! `~` and environment variables inside a folder setting, expanded as
//! Python's `os.path.expandvars(os.path.expanduser(value))` does, so a value
//! the `hf` CLI reads finds the same folder here. Ported from CPython 3.14.6's
//! `ntpath` (Windows) and `posixpath` (elsewhere), with one difference: `~user`
//! stays as written.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// An environment variable's value by name.
pub(crate) type VariableLookup<'a> = &'a dyn Fn(&str) -> Option<OsString>;

/// `value` with a leading `~` replaced by `home`, then its variables by their
/// values. A value, home folder or variable value that is not Unicode is left
/// as written.
pub(crate) fn expand_user_and_variables(
    value: &OsStr,
    home: Option<&Path>,
    variable: VariableLookup<'_>,
) -> PathBuf {
    let Some(text) = value.to_str() else {
        return PathBuf::from(value);
    };
    let home = home.and_then(Path::to_str);
    let lookup = |name: &str| text_variable(variable, name);
    let expanded = if cfg!(windows) {
        nt::expand_variables(&nt::expand_user(text, home), &lookup)
    } else {
        posix::expand_variables(&posix::expand_user(text, home), &lookup)
    };
    PathBuf::from(expanded)
}

/// `None` for a name no environment can hold, as Python's `os.environ` raises
/// `KeyError` for it.
fn text_variable(variable: VariableLookup<'_>, name: &str) -> Option<String> {
    if name.is_empty() || name.contains(['=', '\0']) {
        return None;
    }
    variable(name)?.into_string().ok()
}

/// `ntpath.expanduser` and `ntpath.expandvars`.
mod nt {
    const SEPARATORS: [char; 2] = ['\\', '/'];

    pub(super) fn expand_user(path: &str, home: Option<&str>) -> String {
        match (path.strip_prefix('~'), home) {
            (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with(SEPARATORS) => {
                format!("{home}{rest}")
            }
            _ => path.to_string(),
        }
    }

    /// `%NAME%`, `$NAME` and `${NAME}`; `%%` and `$$` stand for one
    /// character. Text inside single quotes, an unknown variable and an
    /// unclosed form are left as written.
    pub(super) fn expand_variables(
        path: &str,
        variable: &dyn Fn(&str) -> Option<String>,
    ) -> String {
        let mut expanded = String::with_capacity(path.len());
        let mut rest = path;
        while let Some(character) = rest.chars().next() {
            let after = &rest[character.len_utf8()..];
            rest = match character {
                '\'' => match after.find('\'') {
                    Some(close) => {
                        expanded.push('\'');
                        expanded.push_str(&after[..=close]);
                        &after[close + 1..]
                    }
                    None => {
                        expanded.push_str(rest);
                        ""
                    }
                },
                '%' | '$' if after.starts_with(character) => {
                    expanded.push(character);
                    &after[1..]
                }
                '%' => {
                    let (replacement, after_form) =
                        expand_enclosed(rest, after, "%", '%', variable);
                    expanded.push_str(&replacement);
                    after_form
                }
                '$' if after.starts_with('{') => {
                    let (replacement, after_form) =
                        expand_enclosed(rest, &after[1..], "${", '}', variable);
                    expanded.push_str(&replacement);
                    after_form
                }
                '$' => {
                    let name_length = after
                        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
                        .unwrap_or(after.len());
                    let name = &after[..name_length];
                    expanded.push_str(&variable(name).unwrap_or_else(|| format!("${name}")));
                    &after[name_length..]
                }
                _ => {
                    expanded.push(character);
                    after
                }
            };
        }
        expanded
    }

    /// The value of the variable named in `name_and_rest` up to `close`, and
    /// the text after the form. Unclosed, `whole` as written and no text
    /// after it.
    fn expand_enclosed<'a>(
        whole: &'a str,
        name_and_rest: &'a str,
        open: &str,
        close: char,
        variable: &dyn Fn(&str) -> Option<String>,
    ) -> (String, &'a str) {
        let Some(end) = name_and_rest.find(close) else {
            return (whole.to_string(), "");
        };
        let name = &name_and_rest[..end];
        let replacement = variable(name).unwrap_or_else(|| format!("{open}{name}{close}"));
        (replacement, &name_and_rest[end + 1..])
    }
}

/// `posixpath.expanduser` and `posixpath.expandvars`.
mod posix {
    pub(super) fn expand_user(path: &str, home: Option<&str>) -> String {
        match (path.strip_prefix('~'), home) {
            (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with('/') => {
                let expanded = format!("{}{rest}", home.trim_end_matches('/'));
                if expanded.is_empty() {
                    "/".to_string()
                } else {
                    expanded
                }
            }
            _ => path.to_string(),
        }
    }

    /// `$NAME` (ASCII letters, digits and `_`) and `${NAME}`. An unknown
    /// variable is left as written, and so is an unclosed `${` with the text
    /// after it.
    pub(super) fn expand_variables(
        path: &str,
        variable: &dyn Fn(&str) -> Option<String>,
    ) -> String {
        let mut expanded = String::with_capacity(path.len());
        let mut rest = path;
        while let Some(dollar) = rest.find('$') {
            expanded.push_str(&rest[..dollar]);
            let after = &rest[dollar + 1..];
            if after.starts_with('{') && !after.contains('}') {
                rest = &rest[dollar..];
                break;
            }
            let Some((name, form_length)) = variable_form(after) else {
                expanded.push('$');
                rest = after;
                continue;
            };
            let form = &rest[dollar..dollar + 1 + form_length];
            expanded.push_str(&variable(name).unwrap_or_else(|| form.to_string()));
            rest = &after[form_length..];
        }
        expanded.push_str(rest);
        expanded
    }

    /// The name after a `$` and the length of its form.
    fn variable_form(after: &str) -> Option<(&str, usize)> {
        if let Some(braced) = after.strip_prefix('{') {
            let close = braced.find('}')?;
            return Some((&braced[..close], close + 2));
        }
        let length = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(after.len());
        (length > 0).then(|| (&after[..length], length))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn variables(name: &str) -> Option<String> {
        match name {
            "CACHE" => Some("D:\\cache".to_string()),
            "USER_DIR" => Some("u".to_string()),
            "WITH-DASH" => Some("dash".to_string()),
            "TILDE" => Some("~/from-variable".to_string()),
            _ => None,
        }
    }

    /// Expected values printed by CPython 3.14.6's `ntpath.expandvars` with the
    /// variables above as `os.environ`.
    #[test]
    fn windows_variables_expand_as_ntpath_does() {
        let cases = [
            ("%CACHE%\\hub", "D:\\cache\\hub"),
            ("$CACHE\\hub", "D:\\cache\\hub"),
            ("${CACHE}\\hub", "D:\\cache\\hub"),
            ("$WITH-DASH", "dash"),
            (
                "%MISSING%\\$MISSING\\${MISSING}",
                "%MISSING%\\$MISSING\\${MISSING}",
            ),
            ("'%CACHE%'\\%CACHE%", "'%CACHE%'\\D:\\cache"),
            ("'unclosed %CACHE%", "'unclosed %CACHE%"),
            ("100%% $$5", "100% $5"),
            ("%unclosed $CACHE", "%unclosed $CACHE"),
            ("${unclosed $CACHE", "${unclosed $CACHE"),
            ("a$", "a$"),
            ("$\\x", "$\\x"),
            ("%USER_DIR%%CACHE%", "uD:\\cache"),
            ("%TILDE%", "~/from-variable"),
        ];
        for (value, expected) in cases {
            assert_eq!(
                nt::expand_variables(value, &variables),
                expected,
                "{value:?} expands differently from ntpath"
            );
        }
    }

    /// Expected values printed by CPython 3.14.6's `posixpath.expandvars`
    /// with the variables above as `os.environ`.
    #[test]
    fn posix_variables_expand_as_posixpath_does() {
        let cases = [
            ("$CACHE/hub", "D:\\cache/hub"),
            ("${CACHE}/hub", "D:\\cache/hub"),
            ("$WITH-DASH", "$WITH-DASH"),
            ("${WITH-DASH}", "dash"),
            ("$MISSING/${MISSING}", "$MISSING/${MISSING}"),
            ("%CACHE%", "%CACHE%"),
            ("'$CACHE'", "'D:\\cache'"),
            ("$$CACHE", "$D:\\cache"),
            ("${unclosed $CACHE", "${unclosed $CACHE"),
            ("$CACHE/${unclosed", "D:\\cache/${unclosed"),
            ("${}", "${}"),
            ("a$", "a$"),
            ("$TILDE", "~/from-variable"),
        ];
        for (value, expected) in cases {
            assert_eq!(
                posix::expand_variables(value, &variables),
                expected,
                "{value:?} expands differently from posixpath"
            );
        }
    }

    #[test]
    fn windows_tilde_expands_to_home_folder() {
        let home = Some("C:\\Users\\example");
        let cases = [
            ("~", "C:\\Users\\example"),
            ("~\\cache", "C:\\Users\\example\\cache"),
            ("~/cache", "C:\\Users\\example/cache"),
            ("cache\\~", "cache\\~"),
        ];
        for (value, expected) in cases {
            assert_eq!(
                nt::expand_user(value, home),
                expected,
                "{value:?} expands differently from ntpath"
            );
        }
        assert_eq!(
            nt::expand_user("~other\\cache", home),
            "~other\\cache",
            "another user's tilde is expanded"
        );
        assert_eq!(
            nt::expand_user("~\\cache", None),
            "~\\cache",
            "a tilde expands without a home folder"
        );
    }

    #[test]
    fn posix_tilde_expands_to_home_folder() {
        let cases = [
            ("~", Some("/home/example"), "/home/example"),
            ("~/cache", Some("/home/example/"), "/home/example/cache"),
            ("~", Some("/"), "/"),
            ("~\\cache", Some("/home/example"), "~\\cache"),
        ];
        for (value, home, expected) in cases {
            assert_eq!(
                posix::expand_user(value, home),
                expected,
                "{value:?} with home {home:?} expands differently from posixpath"
            );
        }
        assert_eq!(
            posix::expand_user("~other/cache", Some("/home/example")),
            "~other/cache",
            "another user's tilde is expanded"
        );
        assert_eq!(
            posix::expand_user("~/cache", None),
            "~/cache",
            "a tilde expands without a home folder"
        );
    }

    #[test]
    fn tilde_expands_before_variables() {
        let home = Path::new("/home/example");
        let lookup = |name: &str| variables(name).map(OsString::from);

        let from_tilde = expand_user_and_variables(OsStr::new("~/hub"), Some(home), &lookup);
        let from_variable = expand_user_and_variables(
            OsStr::new(if cfg!(windows) { "%TILDE%" } else { "$TILDE" }),
            Some(home),
            &lookup,
        );

        assert_eq!(
            from_tilde,
            PathBuf::from(format!("{}/hub", home.display())),
            "a leading tilde is not replaced by the home folder"
        );
        assert_eq!(
            from_variable,
            PathBuf::from("~/from-variable"),
            "a tilde from a variable's value is expanded"
        );
    }
}
