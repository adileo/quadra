//! The integration tests of quadra-analysis, as one binary (one link instead of one per file):
//! `cargo test -p quadra-analysis --test it [filter]`.

mod clock;
mod depth;
mod frozen;
mod golden;
mod interfaces;
