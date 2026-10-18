//! Slow history reads (search, pages, export) served one at a time by a task of their own, on the
//! read-only connection of a [`Reader`]: the daemon loop only queues them and keeps receiving SMS,
//! calls and commands meanwhile.

use std::path::PathBuf;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::engine::{Cursor, Out};
use crate::history::Reader;
use crate::t;

/// Reads waiting for their turn; beyond, the requester is told to retry.
const QUEUE: usize = 4;
/// Longest wait for room in the requester's channel: past it the reply is dropped (client gone or stuck).
const REPLY_WAIT: Duration = Duration::from_secs(2);

/// A read asked by one client, answered to that client only.
pub enum Job {
    Search { query: String, before: Option<Cursor> },
    Older { key: String, before: Cursor, limit: usize },
    /// Into `dir`, chosen by the daemon (never a path from the IPC).
    Export { key: Option<String>, dir: PathBuf },
}

/// Why a read was not queued.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// Queue full: retry later.
    Busy,
    /// Read task stopped.
    Stopped,
}

impl Refused {
    /// Message for the requester.
    #[must_use]
    pub fn msg(&self) -> String {
        match self {
            Self::Busy => t!("history busy, try again").to_owned(),
            Self::Stopped => t!("cannot read the history").to_owned(),
        }
    }
}

/// Queue of the read task.
#[derive(Clone)]
pub struct Reads(mpsc::Sender<(Job, mpsc::Sender<Out>)>);

impl Reads {
    /// Starts the read task on `reader` (inside a Tokio runtime).
    #[must_use]
    pub fn spawn(reader: Reader) -> Self {
        let (tx, rx) = mpsc::channel(QUEUE);
        tokio::spawn(serve(reader, rx));
        Self(tx)
    }

    /// Queues `job`, answered on `reply`; never waits.
    ///
    /// # Errors
    /// Queue full or read task stopped.
    pub fn submit(&self, job: Job, reply: mpsc::Sender<Out>) -> Result<(), Refused> {
        self.0.try_send((job, reply)).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => Refused::Busy,
            mpsc::error::TrySendError::Closed(_) => Refused::Stopped,
        })
    }
}

async fn serve(mut reader: Reader, mut rx: mpsc::Receiver<(Job, mpsc::Sender<Out>)>) {
    while let Some((job, reply)) = rx.recv().await {
        // The connection goes to the blocking thread and comes back with the answer.
        let Ok((back, out)) = tokio::task::spawn_blocking(move || {
            let out = run(&reader, job);
            (reader, out)
        })
        .await
        else {
            tracing::error!("history read task stopped");
            return;
        };
        reader = back;
        if reply.send_timeout(out, REPLY_WAIT).await.is_err() {
            tracing::warn!("history read: reply dropped (client gone or not reading)");
        }
    }
}

fn run(reader: &Reader, job: Job) -> Out {
    match job {
        Job::Search { query, before } => match reader.search(&query, before.as_ref()) {
            Ok((items, next)) => Out::Hits { query, items, next },
            Err(e) => failed("search", &e),
        },
        Job::Older { key, before, limit } => match reader.older(&key, &before, limit) {
            Ok((items, next)) => Out::Older { key, items, next },
            Err(e) => failed("older", &e),
        },
        Job::Export { key, dir } => match reader.export_to(&dir, key.as_deref()) {
            Ok(path) => {
                tracing::info!("history exported");
                Out::Exported { path: path.display().to_string() }
            }
            Err(e) => {
                tracing::warn!("history (export): {e}");
                Out::Error { msg: t!("cannot export the history").to_owned() }
            }
        },
    }
}

fn failed(what: &str, e: &rusqlite::Error) -> Out {
    tracing::warn!("history ({what}): {e}");
    Out::Error { msg: t!("cannot read the history").to_owned() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addr::BtAddr;
    use crate::history::History;
    use crate::map::Sms;

    fn history() -> History {
        let h = History::memory(&"00:11:22:33:44:55".parse::<BtAddr>().unwrap()).unwrap();
        h.add_sms(&sms("20261015T100000", "été"), false).unwrap();
        h
    }

    fn sms(ts: &str, text: &str) -> Sms {
        Sms { handle: ts.into(), address: "+33600000001".into(), timestamp: ts.into(), text: text.into(), ..Default::default() }
    }

    fn search(q: &str) -> Job {
        Job::Search { query: q.into(), before: None }
    }

    #[tokio::test]
    async fn answers_the_requester_while_the_writer_goes_on() {
        let h = history();
        let reads = Reads::spawn(h.reader().unwrap());
        let (tx, mut rx) = mpsc::channel(1);
        reads.submit(search("ete"), tx).unwrap();
        // Queued without waiting: the writer is free at once.
        assert!(h.add_sms(&sms("20261015T110000", "encore l'été"), false).unwrap());
        let Some(Out::Hits { query, items, .. }) = rx.recv().await else { panic!("no hits") };
        assert_eq!(query, "ete");
        assert!(!items.is_empty());
    }

    #[tokio::test]
    async fn full_queue_is_refused() {
        let h = history();
        let reads = Reads::spawn(h.reader().unwrap());
        let (tx, _rx) = mpsc::channel(8);
        // Single-threaded runtime: the task has not run yet, nothing is dequeued.
        for _ in 0..QUEUE {
            reads.submit(search("a"), tx.clone()).unwrap();
        }
        assert_eq!(reads.submit(search("a"), tx).unwrap_err(), Refused::Busy);
    }

    #[tokio::test]
    async fn read_errors_become_a_message() {
        let h = history();
        let reads = Reads::spawn(h.reader().unwrap());
        let (tx, mut rx) = mpsc::channel(1);
        reads.submit(Job::Export { key: None, dir: "/nonexistent/bluez-message".into() }, tx).unwrap();
        assert!(matches!(rx.recv().await, Some(Out::Error { .. })));
    }
}
