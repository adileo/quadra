//! In-memory interpreter and test runner (phase 5).
//!
//! The interpreter is the reference semantics of Quadra: exact decimals, transactions with
//! rollback, defaults, formulas, constraints at commit, flows, reactions, ledgers with
//! automatic reversals, frozen snapshots, permissions and the chain of causes. Storage on
//! a real database comes later and must behave the same way.

pub mod codec;
pub mod formats;
mod interp;
pub mod limits;
pub mod query;
mod runner;
pub mod store;
pub mod time;
pub mod value;

pub use interp::unique_message;
pub use interp::{
    ChangeDiff, Commit, CommitError, CommitSink, Conflict, Ctx, Flow, Groups, Interp, KeyVal, Page,
    RuleSel, ScriptMeta, Spec, Statics, Units,
};
pub use interp::{FORMULAS_BY_DELTA, FORMULAS_COMPUTED, FORMULAS_STORED};
pub use runner::{run_session, run_tests, run_tests_with, TestBackend, TestResult};
pub use store::{Cause, LedgerRow, ReadSet, Record, SharedSource, Source, Store};
pub use value::V;
pub use value::{Closure, Id};
