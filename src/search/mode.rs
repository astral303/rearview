use clap::ValueEnum;
use serde::Deserialize;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum SearchMode {
    /// Every unquoted word must match, as a word or the start of one
    #[default]
    Lexical,
    /// Rank messages by meaning
    Semantic,
    /// Match the whole query as one phrase. Case-sensitive only when the
    /// query has an uppercase letter; spacing and line breaks need not match
    Exact,
    /// Combine lexical and semantic message ranking
    Hybrid,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SearchModeResolution {
    pub cli_mode: Option<SearchMode>,
    pub config_mode: Option<SearchMode>,
    pub tui_semantic_search: Option<bool>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TuiSearchMode {
    #[default]
    Lexical,
    Semantic,
}

pub fn resolve_search_mode(resolution: SearchModeResolution) -> SearchMode {
    resolution
        .cli_mode
        .or(resolution.config_mode)
        .or_else(|| resolution.tui_semantic_search.map(legacy_tui_mode))
        .unwrap_or_default()
}

pub fn resolve_tui_search_mode(resolution: SearchModeResolution) -> TuiSearchMode {
    match resolve_search_mode(resolution) {
        SearchMode::Semantic => TuiSearchMode::Semantic,
        SearchMode::Lexical | SearchMode::Exact | SearchMode::Hybrid => TuiSearchMode::Lexical,
    }
}

fn legacy_tui_mode(semantic_search: bool) -> SearchMode {
    if semantic_search {
        SearchMode::Semantic
    } else {
        SearchMode::Lexical
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_mode_config_wins_over_tui_semantic_search() {
        let mode = resolve_search_mode(SearchModeResolution {
            cli_mode: None,
            config_mode: Some(SearchMode::Lexical),
            tui_semantic_search: Some(true),
        });

        assert_eq!(mode, SearchMode::Lexical);
    }

    #[test]
    fn cli_mode_wins_over_search_mode_config() {
        let mode = resolve_search_mode(SearchModeResolution {
            cli_mode: Some(SearchMode::Exact),
            config_mode: Some(SearchMode::Semantic),
            tui_semantic_search: Some(true),
        });

        assert_eq!(mode, SearchMode::Exact);
    }

    #[test]
    fn tui_semantic_search_aliases_to_semantic() {
        let mode = resolve_search_mode(SearchModeResolution {
            cli_mode: None,
            config_mode: None,
            tui_semantic_search: Some(true),
        });

        assert_eq!(mode, SearchMode::Semantic);
    }

    #[test]
    fn tui_semantic_search_false_aliases_to_lexical() {
        let mode = resolve_search_mode(SearchModeResolution {
            cli_mode: None,
            config_mode: None,
            tui_semantic_search: Some(false),
        });

        assert_eq!(mode, SearchMode::Lexical);
    }

    #[test]
    fn unset_search_mode_defaults_to_lexical() {
        assert_eq!(
            resolve_search_mode(SearchModeResolution::default()),
            SearchMode::Lexical
        );
    }

    #[test]
    fn tui_resolution_keeps_supported_semantic_mode() {
        let mode = resolve_tui_search_mode(SearchModeResolution {
            cli_mode: None,
            config_mode: Some(SearchMode::Semantic),
            tui_semantic_search: Some(false),
        });

        assert_eq!(mode, TuiSearchMode::Semantic);
    }

    #[test]
    fn tui_resolution_maps_hybrid_to_lexical() {
        let mode = resolve_tui_search_mode(SearchModeResolution {
            cli_mode: None,
            config_mode: Some(SearchMode::Hybrid),
            tui_semantic_search: Some(true),
        });

        assert_eq!(mode, TuiSearchMode::Lexical);
    }

    #[test]
    fn tui_resolution_maps_exact_to_lexical() {
        let mode = resolve_tui_search_mode(SearchModeResolution {
            cli_mode: None,
            config_mode: Some(SearchMode::Exact),
            tui_semantic_search: Some(true),
        });

        assert_eq!(mode, TuiSearchMode::Lexical);
    }
}
