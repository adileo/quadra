//! The on-premise agents of a tenant (`quadra agent`), as the server sees them: the jobs
//! waiting for each, the answers awaited, and who was seen when.
//!
//! An agent only ever opens connections to the server, never the other way: it asks for its
//! jobs (`GET /v1/agent/jobs`, held open until there is one or `wait` passes) and sends what
//! came of them (`POST /v1/agent/results`). A job is given to one agent and answered once; a
//! job not answered within its time is said to have failed, to be tried again. Revoking an
//! agent drops what it waits for at once: it receives nothing more.

use crate::transport::{Failed, Job, Outcome};
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::{oneshot, Notify};

/// How long an agent unseen still counts as connected (it asks again within `wait`).
const ALIVE: Duration = Duration::from_secs(60);
/// How long a job waits for its answer.
const ANSWER: Duration = Duration::from_secs(600);

#[derive(Default)]
struct State {
    next: u64,
    /// per agent, the jobs not taken yet
    queued: HashMap<String, VecDeque<(u64, Job)>>,
    /// the jobs given (or queued), waiting for their answer: (agent, its answer)
    waiting: HashMap<u64, (String, oneshot::Sender<Outcome>)>,
    /// per agent: when it last asked, and its version
    seen: HashMap<String, (Instant, String)>,
    /// agents revoked since the server started (the database says it too)
    revoked: Vec<String>,
}

/// The agents of one tenant.
#[derive(Default)]
pub struct Agents {
    state: Mutex<State>,
    /// one per agent: a job for it, or it was revoked
    bell: Mutex<HashMap<String, std::sync::Arc<Notify>>>,
}

/// An agent as the studio shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seen {
    pub name: String,
    pub version: String,
    pub ago: Duration,
    pub connected: bool,
}

impl Agents {
    fn bell(&self, name: &str) -> std::sync::Arc<Notify> {
        self.bell
            .lock()
            .unwrap()
            .entry(name.to_string())
            .or_default()
            .clone()
    }

    /// Runs `job` on the agent `name`: queued for it, and its answer awaited.
    pub async fn run(&self, name: &str, job: &Job) -> Outcome {
        let (tx, rx) = oneshot::channel();
        let id = {
            let mut s = self.state.lock().unwrap();
            if s.revoked.iter().any(|r| r == name) {
                return Outcome::Failed(Failed::Waiting(format!(
                    "the agent `{name}` was revoked: register another one (`quadra agent add {name}`)"
                )));
            }
            let alive = s.seen.get(name).is_some_and(|(at, _)| at.elapsed() < ALIVE);
            if !alive {
                return Outcome::Failed(Failed::Again(format!(
                    "the agent `{name}` is not connected"
                )));
            }
            s.next += 1;
            let id = s.next;
            s.queued
                .entry(name.to_string())
                .or_default()
                .push_back((id, job.clone()));
            s.waiting.insert(id, (name.to_string(), tx));
            id
        };
        self.bell(name).notify_one();
        match tokio::time::timeout(ANSWER, rx).await {
            Ok(Ok(o)) => o,
            Ok(Err(_)) => Outcome::Failed(Failed::Again(format!(
                "the agent `{name}` went away before answering"
            ))),
            Err(_) => {
                self.forget(id);
                Outcome::Failed(Failed::Again(format!(
                    "no answer from the agent `{name}` within {} s",
                    ANSWER.as_secs()
                )))
            }
        }
    }

    fn forget(&self, id: u64) {
        let mut s = self.state.lock().unwrap();
        s.waiting.remove(&id);
        for q in s.queued.values_mut() {
            q.retain(|(i, _)| *i != id);
        }
    }

    /// The jobs of the agent `name`, waiting up to `wait` for one. None: it was revoked.
    pub async fn take(&self, name: &str, version: &str, wait: Duration) -> Option<Vec<(u64, Job)>> {
        let bell = self.bell(name);
        let until = Instant::now() + wait;
        loop {
            {
                let mut s = self.state.lock().unwrap();
                if s.revoked.iter().any(|r| r == name) {
                    return None;
                }
                s.seen
                    .insert(name.to_string(), (Instant::now(), version.to_string()));
                if let Some(q) = s.queued.get_mut(name) {
                    if !q.is_empty() {
                        return Some(q.drain(..).collect());
                    }
                }
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Some(Vec::new());
            }
            let _ = tokio::time::timeout(left, bell.notified()).await;
        }
    }

    /// What came of job `id`, from the agent `name` (a job of another agent is ignored).
    pub fn answer(&self, name: &str, id: u64, outcome: Outcome) -> bool {
        let mut s = self.state.lock().unwrap();
        match s.waiting.get(&id) {
            Some((a, _)) if a == name => {
                let (_, tx) = s.waiting.remove(&id).unwrap();
                let _ = tx.send(outcome);
                true
            }
            _ => false,
        }
    }

    /// The agent `name` revoked: its jobs fail, and what it waits for ends now.
    pub fn revoke(&self, name: &str) {
        {
            let mut s = self.state.lock().unwrap();
            if !s.revoked.iter().any(|r| r == name) {
                s.revoked.push(name.to_string());
            }
            s.queued.remove(name);
            s.seen.remove(name);
            let ids: Vec<u64> = s
                .waiting
                .iter()
                .filter(|(_, (a, _))| a == name)
                .map(|(i, _)| *i)
                .collect();
            for id in ids {
                if let Some((_, tx)) = s.waiting.remove(&id) {
                    let _ = tx.send(Outcome::Failed(Failed::Waiting(format!(
                        "the agent `{name}` was revoked"
                    ))));
                }
            }
        }
        self.bell(name).notify_waiters();
    }

    /// An agent registered again under a revoked name.
    pub fn restore(&self, name: &str) {
        self.state.lock().unwrap().revoked.retain(|r| r != name);
    }

    /// The agents seen since the server started.
    pub fn seen(&self) -> Vec<Seen> {
        let s = self.state.lock().unwrap();
        let mut out: Vec<Seen> = s
            .seen
            .iter()
            .map(|(n, (at, v))| Seen {
                name: n.clone(),
                version: v.clone(),
                ago: at.elapsed(),
                connected: at.elapsed() < ALIVE,
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{Conn, Put};
    use std::sync::Arc;

    fn job() -> Job {
        Job::Put {
            conn: Conn {
                name: "erp".into(),
                toml: String::new(),
            },
            put: Put::File {
                path: "a.txt".into(),
                body: b"hi".to_vec(),
            },
        }
    }

    #[tokio::test]
    async fn a_job_goes_to_its_agent_and_its_answer_comes_back() {
        let a = Arc::new(Agents::default());
        // not connected: tried again later
        assert!(matches!(
            a.run("milano", &job()).await,
            Outcome::Failed(Failed::Again(_))
        ));
        // connected (it asked once): the job reaches it, its answer comes back
        assert_eq!(a.take("milano", "0.1", Duration::ZERO).await, Some(vec![]));
        let a2 = a.clone();
        let agent = tokio::spawn(async move {
            let jobs = a2
                .take("milano", "0.1", Duration::from_secs(5))
                .await
                .unwrap();
            assert_eq!(jobs.len(), 1);
            // another agent cannot answer it
            assert!(!a2.answer("roma", jobs[0].0, Outcome::Put));
            assert!(a2.answer("milano", jobs[0].0, Outcome::Put));
        });
        assert_eq!(a.run("milano", &job()).await, Outcome::Put);
        agent.await.unwrap();
        assert_eq!(a.seen()[0].name, "milano");
    }

    #[tokio::test]
    async fn a_revoked_agent_receives_nothing_more_at_once() {
        let a = Arc::new(Agents::default());
        a.take("milano", "0.1", Duration::ZERO).await;
        let a2 = a.clone();
        let waiting = tokio::spawn(async move {
            let t = Instant::now();
            let r = a2.take("milano", "0.1", Duration::from_secs(30)).await;
            (r, t.elapsed())
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        a.revoke("milano");
        let (r, took) = waiting.await.unwrap();
        assert_eq!(r, None);
        assert!(took < Duration::from_secs(5), "{took:?}");
        assert!(matches!(
            a.run("milano", &job()).await,
            Outcome::Failed(Failed::Waiting(_))
        ));
    }
}
