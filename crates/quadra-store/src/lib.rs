//! Storage for Quadra on SQLite and PostgreSQL (interchangeable).
//!
//! The engine stays the reference semantics; the database stores what it commits:
//! the truth (every write of every field, ledger rows, a signed commit log) and readable
//! projections (one table per entity, with real constraints and materialized formulas).

pub mod agents;
pub mod auth;
pub mod bulk;
pub mod codec;
pub mod conn;
pub mod db;
pub mod deliveries;
pub mod deploy;
pub mod files;
pub mod grants;
pub mod hints;
pub mod schema;
pub mod segments;
pub mod sequencer;
pub mod source;
pub mod sqlq;
pub mod verify;
pub mod views;
pub mod writer;

pub use conn::{Conn, Dialect};
pub mod ops;
pub use db::{materialized_formulas, Database, OutboxEvent, Sink, Status};
pub use sequencer::{SequencedSink, Sequencer};
pub use source::{Cache, DbSource};

mod testing;
pub use testing::DbBackend;
