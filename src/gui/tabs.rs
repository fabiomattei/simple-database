//! One module per tab, each an `impl App` block with the tab's `tab_*` method
//! and the actions that belong to it.

mod data;
mod diagram;
mod history;
mod migrate;
mod query;
mod schema;
mod seed;
mod sql;

pub(crate) use history::HistoryState;
