//! The commands of one TCF connection, between the thread that reads them and
//! the one that runs them.
//!
//! Commands run one at a time, in order. A frontend sends them as soon as it
//! has them, so a resume can arrive behind a pile of reads (variables, memory
//! views, live watches) and would wait for all of them, about 1.2 ms each on a
//! BlueBox. Their answers describe the halted state the resume ends, and a
//! frontend throws them away. So a resume, a step (also `RunControl.resume`)
//! or a reset takes the reads still waiting before it out of the queue: they
//! are answered at once as "Execution context is running", and the run
//! command comes next. Everything else keeps its place, writes in particular:
//! a write sent before a resume still happens before it.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};

/// Commands that the reads before them are dropped for.
const RUN_COMMANDS: &[(&str, &str)] = &[("RunControl", "resume"), ("ContextReset", "reset")];

/// Commands that only read the target.
const READS: &[(&str, &str)] = &[
    ("Memory", "get"),
    ("Registers", "get"),
    ("StackTrace", "getChildren"),
    ("StackTrace", "getChildrenRange"),
    ("StackTrace", "getContext"),
    ("Expressions", "evaluate"),
    ("AurixOS", "getObjects"),
    ("AurixOS", "getRunning"),
    ("AurixOS", "getStacks"),
    ("AurixTarget", "getLiveValues"),
];

/// Service and command of a command message (`C token service command ...`).
pub fn command(m: &[String]) -> Option<(&str, &str)> {
    match m {
        [c, _, service, command, ..] if c == "C" => Some((service.as_str(), command.as_str())),
        _ => None,
    }
}

fn is(m: &[String], list: &[(&str, &str)]) -> bool {
    command(m).is_some_and(|c| list.contains(&c))
}

#[derive(Default)]
pub struct Queue {
    inner: Mutex<Inner>,
    ready: Condvar,
}

#[derive(Default)]
struct Inner {
    messages: VecDeque<Vec<String>>,
    closed: bool,
}

impl Queue {
    /// Queues a message. A run command takes the reads queued before it out;
    /// they are returned, for the caller to answer.
    pub fn push(&self, m: Vec<String>) -> Vec<Vec<String>> {
        let mut g = self.inner.lock().unwrap();
        let mut dropped = Vec::new();
        if is(&m, RUN_COMMANDS) {
            let (reads, keep): (VecDeque<_>, VecDeque<_>) = g.messages.drain(..).partition(|q| is(q, READS));
            g.messages = keep;
            dropped.extend(reads);
        }
        g.messages.push_back(m);
        self.ready.notify_one();
        dropped
    }

    /// The next message; `None` once the queue is closed and empty.
    pub fn pop(&self) -> Option<Vec<String>> {
        let mut g = self.inner.lock().unwrap();
        loop {
            if let Some(m) = g.messages.pop_front() {
                return Some(m);
            }
            if g.closed {
                return None;
            }
            g = self.ready.wait(g).unwrap();
        }
    }

    /// No more messages come; [`pop`](Self::pop) returns the rest, then `None`.
    pub fn close(&self) {
        self.inner.lock().unwrap().closed = true;
        self.ready.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(token: &str, service: &str, cmd: &str) -> Vec<String> {
        vec!["C".into(), token.into(), service.into(), cmd.into(), "\"ctx\"".into()]
    }

    fn tokens(q: &Queue) -> Vec<String> {
        q.close();
        std::iter::from_fn(|| q.pop()).map(|m| m[1].clone()).collect()
    }

    #[test]
    fn a_resume_drops_the_reads_before_it_and_keeps_the_rest_in_order() {
        let q = Queue::default();
        assert!(q.push(c("1", "Memory", "get")).is_empty());
        q.push(c("2", "Memory", "set"));
        q.push(c("3", "StackTrace", "getChildren"));
        q.push(c("4", "Breakpoints", "add"));
        q.push(c("5", "Expressions", "evaluate"));
        let dropped = q.push(c("6", "RunControl", "resume"));
        assert_eq!(dropped.iter().map(|m| m[1].as_str()).collect::<Vec<_>>(), ["1", "3", "5"]);
        q.push(c("7", "Memory", "get"));
        assert_eq!(tokens(&q), ["2", "4", "6", "7"], "a read after the resume stays");
    }

    #[test]
    fn a_reset_drops_reads_too_and_other_commands_drop_nothing() {
        let q = Queue::default();
        q.push(c("1", "Registers", "get"));
        assert!(q.push(c("2", "RunControl", "suspend")).is_empty());
        assert!(q.push(c("3", "Memory", "set")).is_empty());
        assert_eq!(q.push(c("4", "ContextReset", "reset")).len(), 1);
        assert_eq!(tokens(&q), ["2", "3", "4"]);
    }

    #[test]
    fn other_messages_pass_through() {
        let q = Queue::default();
        q.push(vec!["E".into(), "Locator".into(), "Hello".into()]);
        q.push(c("1", "Memory", "get"));
        assert_eq!(q.push(c("2", "RunControl", "resume")).len(), 1);
        q.close();
        assert_eq!(q.pop().unwrap()[0], "E");
        assert_eq!(q.pop().unwrap()[1], "2");
        assert!(q.pop().is_none());
    }

    #[test]
    fn pop_waits_for_a_push() {
        let q = std::sync::Arc::new(Queue::default());
        let q2 = q.clone();
        let t = std::thread::spawn(move || q2.pop());
        std::thread::sleep(std::time::Duration::from_millis(20));
        q.push(c("1", "Memory", "get"));
        assert_eq!(t.join().unwrap().unwrap()[1], "1");
    }
}
