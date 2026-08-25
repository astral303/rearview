//! Filters that narrow what a load returns.

/// One term of a filter, as the list reports it: what was bounded, and what it
/// was bounded to.
///
/// Two parts rather than a sentence, so the list can set them out in columns
/// the way it sets out keys and their actions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FilterTerm {
    pub label: String,
    pub value: String,
}

impl FilterTerm {
    pub fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
        }
    }
}

/// `label: value`, for output that has no columns.
impl std::fmt::Display for FilterTerm {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.label, self.value)
    }
}

/// A filter applied while history loads, named for the user.
///
/// The TUI reports these when the list holds less than the user expects, so a
/// filter that keeps sessions out of the list belongs here.
pub trait HistoryFilter {
    /// This filter's terms, or nothing when it constrains nothing.
    fn describe(&self) -> Vec<FilterTerm>;
}

/// Every term in force, named for the user.
pub fn active_load_filters(filters: &[&dyn HistoryFilter]) -> Vec<FilterTerm> {
    filters
        .iter()
        .flat_map(|filter| filter.describe())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed(Vec<FilterTerm>);

    impl HistoryFilter for Fixed {
        fn describe(&self) -> Vec<FilterTerm> {
            self.0.clone()
        }
    }

    #[test]
    fn only_the_filters_that_constrain_anything_contribute_terms() {
        let bounded = Fixed(vec![FilterTerm::new("since", "2026-08-17 13:45")]);
        let unbounded = Fixed(Vec::new());

        assert_eq!(
            active_load_filters(&[&bounded, &unbounded]),
            vec![FilterTerm::new("since", "2026-08-17 13:45")]
        );
        assert!(active_load_filters(&[&unbounded]).is_empty());
    }
}
