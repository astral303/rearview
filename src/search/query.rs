use crate::search::literal::Literal;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedQuery {
    raw: String,
    unquoted: String,
    literals: Vec<Literal>,
}

impl ParsedQuery {
    pub fn parse(query: &str) -> Self {
        let mut unquoted = String::new();
        let mut literals = Vec::new();
        let mut literal = String::new();
        let mut in_quote = false;

        for ch in query.chars() {
            if ch == '"' {
                if in_quote {
                    if !literal.trim().is_empty() {
                        literals.push(Literal::new(literal.clone()));
                    }
                    literal.clear();
                    in_quote = false;
                } else {
                    in_quote = true;
                }
            } else if in_quote {
                literal.push(ch);
            } else {
                unquoted.push(ch);
            }
        }

        if in_quote && !literal.trim().is_empty() {
            literals.push(Literal::new(literal));
        }

        Self {
            raw: query.to_string(),
            unquoted: unquoted.trim().to_string(),
            literals,
        }
    }

    #[cfg(test)]
    pub fn raw(&self) -> &str {
        &self.raw
    }

    pub fn unquoted(&self) -> &str {
        &self.unquoted
    }

    pub fn lexical_text(&self) -> &str {
        if self.is_effectively_empty() {
            ""
        } else if self.unquoted.is_empty() {
            self.raw.trim()
        } else {
            &self.unquoted
        }
    }

    pub fn semantic_text(&self) -> &str {
        &self.unquoted
    }

    pub fn is_effectively_empty(&self) -> bool {
        self.literals.is_empty() && self.unquoted.split_whitespace().next().is_none()
    }

    pub fn literals(&self) -> &[Literal] {
        &self.literals
    }

    pub fn is_quoted_only(&self) -> bool {
        !self.literals.is_empty() && self.unquoted.split_whitespace().next().is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::literal::CaseMode;

    #[test]
    fn parses_quoted_phrase() {
        let parsed = ParsedQuery::parse("\"exact phrase\"");
        assert_eq!(parsed.unquoted(), "");
        assert_eq!(parsed.lexical_text(), "\"exact phrase\"");
        assert_eq!(parsed.semantic_text(), "");
        assert!(parsed.is_quoted_only());
        assert_eq!(parsed.literals()[0].text(), "exact phrase");
        assert_eq!(parsed.literals()[0].case_mode(), CaseMode::Insensitive);
    }

    #[test]
    fn parses_mixed_text_and_literals() {
        let parsed = ParsedQuery::parse("alpha \"Beta Gamma\" delta");
        assert_eq!(parsed.unquoted(), "alpha  delta");
        assert_eq!(parsed.lexical_text(), "alpha  delta");
        assert_eq!(parsed.semantic_text(), "alpha  delta");
        assert!(!parsed.is_quoted_only());
        assert_eq!(parsed.literals()[0].text(), "Beta Gamma");
        assert_eq!(parsed.literals()[0].case_mode(), CaseMode::Sensitive);
    }

    #[test]
    fn parses_trailing_open_quote_as_literal() {
        let parsed = ParsedQuery::parse("alpha \"open phrase");
        assert_eq!(parsed.unquoted(), "alpha");
        assert_eq!(parsed.literals()[0].text(), "open phrase");
    }

    #[test]
    fn drops_empty_quotes() {
        let parsed = ParsedQuery::parse("\"\"");
        assert!(parsed.literals().is_empty());
        assert!(!parsed.is_quoted_only());
        assert!(parsed.is_effectively_empty());
        assert_eq!(parsed.lexical_text(), "");
    }

    #[test]
    fn drops_whitespace_only_quotes() {
        let parsed = ParsedQuery::parse("\"  \t \"");
        assert!(parsed.literals().is_empty());
        assert!(!parsed.is_quoted_only());
        assert!(parsed.is_effectively_empty());
        assert_eq!(parsed.lexical_text(), "");
    }

    #[test]
    fn drops_trailing_empty_open_quote() {
        let parsed = ParsedQuery::parse("alpha \"");
        assert_eq!(parsed.unquoted(), "alpha");
        assert!(parsed.literals().is_empty());
    }

    #[test]
    fn parses_quoted_uuid_as_literal() {
        let parsed = ParsedQuery::parse("\"e7d318b1-4274-4ee2-a341-e94893b5df49\"");
        assert_eq!(
            parsed.literals()[0].text(),
            "e7d318b1-4274-4ee2-a341-e94893b5df49"
        );
        assert_eq!(parsed.semantic_text(), "");
    }

    #[test]
    fn assigns_smart_case_per_literal() {
        let parsed = ParsedQuery::parse("\"lower phrase\" \"Upper Phrase\"");
        assert_eq!(parsed.literals()[0].case_mode(), CaseMode::Insensitive);
        assert_eq!(parsed.literals()[1].case_mode(), CaseMode::Sensitive);
    }
}
