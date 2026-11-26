//! The integration tests of quadra-cli, as one binary (one link instead of one per file):
//! `cargo test -p quadra-cli --test it [filter]`.

mod app_components;
mod apps;
mod complete;
mod folders;
mod formulas;
mod groups;
mod interfaces;
mod load;
mod lsp;
mod permissions;
mod query;
mod views;
