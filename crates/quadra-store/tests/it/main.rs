//! The integration tests of quadra-store, as one binary (one link instead of one per file):
//! `cargo test -p quadra-store --test it [filter]`.

mod crash;
mod deploy;
mod due;
mod hints;
mod import;
mod limits;
mod sequencer;
mod storage;
