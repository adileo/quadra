//! The integration tests of quadra-runtime, as one binary (one link instead of one per file):
//! `cargo test -p quadra-runtime --test it [filter]`.

mod examples;
mod integrations;
mod regressions;
mod timezones;
