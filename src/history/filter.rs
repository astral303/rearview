//! Filters that narrow what a load returns.

/// A filter applied while history loads, named for the user.
///
/// The TUI reports these when the list holds less than the user expects, so a
/// filter that keeps sessions out of the list belongs here.
pub trait HistoryFilter {
    /// How this filter reads on screen, or `None` when it constrains nothing.
    fn describe(&self) -> Option<String>;
}

/// Every filter in force, named for the user.
pub fn active_load_filters(filters: &[&dyn HistoryFilter]) -> Vec<String> {
    filters
        .iter()
        .filter_map(|filter| filter.describe())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed(Option<&'static str>);

    impl HistoryFilter for Fixed {
        fn describe(&self) -> Option<String> {
            self.0.map(str::to_owned)
        }
    }

    #[test]
    fn only_the_filters_that_constrain_anything_are_named() {
        let bounded = Fixed(Some("since 2026-08-17 13:45"));
        let unbounded = Fixed(None);

        assert_eq!(
            active_load_filters(&[&bounded, &unbounded]),
            vec!["since 2026-08-17 13:45".to_string()]
        );
        assert!(active_load_filters(&[&unbounded]).is_empty());
    }
}
