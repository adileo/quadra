//! Limits of a job: how long it may run, how many rows a statement may return, how long a
//! list may grow. They belong to the thread that runs the job (an engine is one thread), so
//! the jobs of one engine never slow down those of another.
//!
//! The engine sets them with `begin` before each job. Interpreters created on that thread
//! take the deadline and the list cap; the database layer interrupts statements past the
//! deadline and stops reading past the row cap. A limit that is hit is recorded (`hit`), so
//! that the engine answers with an error even where a failure would otherwise only leave a
//! value missing.

use std::cell::{Cell, RefCell};
use std::time::Instant;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Limits {
    /// the job stops with an error after this instant
    pub deadline: Option<Instant>,
    /// rows a single statement may return
    pub max_rows: Option<usize>,
    /// elements a list or a set may have
    pub max_list: Option<usize>,
    /// users (`User` records) the project may have: creating one more fails
    pub max_users: Option<u64>,
}

pub const TIMEOUT: &str = "timeout: the request took longer than its time limit (simplify the formula or filter the rows first)";

pub fn too_many_rows(max: usize) -> String {
    format!("too many rows: a query read more than {max} rows (filter the rows first)")
}

pub fn too_long_list(max: usize) -> String {
    format!("too many elements: a list grew beyond {max} elements (filter the rows first)")
}

thread_local! {
    static LIMITS: Cell<Limits> = const { Cell::new(Limits { deadline: None, max_rows: None, max_list: None, max_users: None }) };
    static STEPS: Cell<u64> = const { Cell::new(0) };
    static ROWS_READ: Cell<u64> = const { Cell::new(0) };
    static ROWS_WRITTEN: Cell<u64> = const { Cell::new(0) };
    static HIT: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// The limits of the job running on this thread.
pub fn current() -> Limits {
    LIMITS.with(|l| l.get())
}

/// Sets the limits of a job on this thread and forgets the limit hit by the previous one.
/// The previous limits come back when the guard is dropped.
pub fn begin(limits: Limits) -> Guard {
    HIT.with(|h| *h.borrow_mut() = None);
    Guard { previous: LIMITS.with(|l| l.replace(limits)) }
}

pub struct Guard {
    previous: Limits,
}

impl Drop for Guard {
    fn drop(&mut self) {
        LIMITS.with(|l| l.set(self.previous));
    }
}

/// Runs `f` without the row cap: the engine's own bookkeeping (the cache catching up with
/// a large commit) is not a request reading too much.
pub fn without_row_cap<T>(f: impl FnOnce() -> T) -> T {
    let before = current();
    LIMITS.with(|l| l.set(Limits { max_rows: None, ..before }));
    let out = f();
    LIMITS.with(|l| l.set(Limits { max_rows: before.max_rows, ..current() }));
    out
}

pub fn too_many_users(max: u64) -> String {
    format!("too many users: this project may have {max} (remove one, or raise the limit)")
}

/// What the work of this thread used since it was last taken: the measure of compute that a
/// host may count and bill. It does not depend on the machine or its load: the same request
/// on the same data counts the same.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    /// expressions evaluated by the interpreter
    pub steps: u64,
    /// rows returned by the database
    pub rows_read: u64,
    /// rows inserted, changed or deleted
    pub rows_written: u64,
}

#[inline]
pub fn count_step() {
    STEPS.with(|c| c.set(c.get().wrapping_add(1)));
}

pub fn count_read(rows: u64) {
    ROWS_READ.with(|c| c.set(c.get().wrapping_add(rows)));
}

pub fn count_written(rows: u64) {
    ROWS_WRITTEN.with(|c| c.set(c.get().wrapping_add(rows)));
}

/// Runs `f` without counting what it uses: the engine's own bookkeeping (its caches catching
/// up), which depends on what ran before rather than on the request.
pub fn uncounted<T>(f: impl FnOnce() -> T) -> T {
    let before = (STEPS.with(Cell::get), ROWS_READ.with(Cell::get), ROWS_WRITTEN.with(Cell::get));
    let out = f();
    STEPS.with(|c| c.set(before.0));
    ROWS_READ.with(|c| c.set(before.1));
    ROWS_WRITTEN.with(|c| c.set(before.2));
    out
}

/// The usage of this thread so far, and the counters back to zero.
pub fn take_usage() -> Usage {
    Usage { steps: STEPS.with(|c| c.replace(0)), rows_read: ROWS_READ.with(|c| c.replace(0)), rows_written: ROWS_WRITTEN.with(|c| c.replace(0)) }
}

/// The job's deadline has passed.
pub fn expired() -> bool {
    current().deadline.is_some_and(|d| Instant::now() > d)
}

/// Records that a limit of the job was hit (the first one is kept).
pub fn hit(message: impl Into<String>) {
    HIT.with(|h| {
        let mut h = h.borrow_mut();
        if h.is_none() {
            *h = Some(message.into());
        }
    });
}

/// The limit hit by the job so far, if any.
pub fn take_hit() -> Option<String> {
    HIT.with(|h| h.borrow_mut().take())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn usage_is_counted_per_thread_and_taken_once() {
        take_usage();
        count_step();
        count_step();
        count_read(5);
        count_written(2);
        let other = std::thread::spawn(|| {
            count_step();
            take_usage()
        })
        .join()
        .unwrap();
        assert_eq!(other, Usage { steps: 1, rows_read: 0, rows_written: 0 });
        assert_eq!(take_usage(), Usage { steps: 2, rows_read: 5, rows_written: 2 });
        assert_eq!(take_usage(), Usage::default());
    }

    #[test]
    fn limits_are_per_job_and_restored() {
        assert_eq!(current(), Limits::default());
        {
            let _g = begin(Limits { max_rows: Some(10), ..Default::default() });
            assert_eq!(current().max_rows, Some(10));
            without_row_cap(|| assert_eq!(current().max_rows, None));
            assert_eq!(current().max_rows, Some(10));
            hit("first");
            hit("second");
            assert_eq!(take_hit().as_deref(), Some("first"));
            assert_eq!(take_hit(), None);
            hit("left over");
        }
        assert_eq!(current(), Limits::default());
        let _g = begin(Limits { deadline: Some(Instant::now() - Duration::from_millis(1)), ..Default::default() });
        assert!(expired());
        assert_eq!(take_hit(), None, "a new job starts with no limit hit");
    }
}
