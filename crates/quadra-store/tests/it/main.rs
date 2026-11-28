//! The integration tests of quadra-store, as one binary (one link instead of one per file):
//! `cargo test -p quadra-store --test it [filter]`.

mod archive;
mod crash;
mod deletion;
mod deploy;
mod due;
mod erasure;
mod hints;
mod history;
mod import;
mod limits;
mod messages;
mod sequencer;
mod signing;
mod storage;
