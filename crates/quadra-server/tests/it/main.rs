//! The integration tests of quadra-server, as one binary (one link instead of one per file):
//! `cargo test -p quadra-server --test it [filter]`.

mod api;
mod databases;
mod deliver;
mod host;
mod orchestration;
mod signin;
