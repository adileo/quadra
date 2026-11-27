//! The integration tests of quadra-server, as one binary (one link instead of one per file):
//! `cargo test -p quadra-server --test it [filter]`.

mod agents;
mod api;
mod deliver;
mod host;
mod mail;
mod orchestration;
mod signin;
