mod actions_menu;
mod app;
mod backend;
mod command_tags;
#[cfg(test)]
pub(crate) mod contract_outputs;
mod export;
#[cfg(test)]
pub(crate) mod output_coverage;
mod runtime;
pub mod search;
mod semantic_worker;
mod supervisor;
pub mod theme;
mod ui;
pub mod viewer;

#[cfg(test)]
pub(crate) use actions_menu::SessionAction;
#[cfg(test)]
pub(crate) use app::claude_home;
pub use app::{Action, ListScope, ListSearchMode, TuiSearchOptions};
pub(crate) use command_tags::{parse_command_name, parse_command_name_and_args};
pub(crate) use export::{ExportOptions, plain_text};
pub use runtime::{run_single_file, run_with_loader};
pub(crate) use ui::format_tokens;
pub use viewer::{RenderOptions, ToolDisplayMode};
