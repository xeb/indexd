//! SQLite command log — one row per spoken turn, from `queued` to a terminal
//! state.
//!
//! The database lives under `~/.local/share/indexd` (see [`crate::config`]),
//! never under the project directory: that path is an NTFS/fuseblk mount where
//! permissions are advisory, and this file holds a transcript of everything
//! said to the ring plus everything the agent answered.
//!
//! `run_agent` is fire-and-forget (ORIGINAL_SPEC §1, §5), so this table is the
//! *only* place an outcome is ever recorded — the tool return carries nothing
//! but `"queued a1b2"`. Part II §II.7 fixes the mapping from outcome to row:
//!
//! | outcome                      | `status`    | `reply`        |
//! |------------------------------|-------------|----------------|
//! | both tags found              | `done`      | extracted body |
//! | opener + 3 idle polls        | `done`      | recovered body |
//! | 600s elapsed                 | `timed_out` | null           |
//! | tmux/send failure            | `failed`    | null, `error` set |
//! | daemon restarted mid-turn    | `failed`    | null, `error` = [`STRANDED_ERROR`] |
//!
//! Every write goes through one `Mutex<Connection>`. The worker is a single
//! FIFO task and the console is read-mostly, so contention is not a concern;
//! correctness under a restart is, which is what [`Db::reconcile_stranded`] is
//! for.

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// What a row left in flight by a restart is settled with.
///
/// Spelled once, here, because the console renders it verbatim and the
/// reconcile test asserts on it.
pub const STRANDED_ERROR: &str = "interrupted by a restart";

/// The schema, exactly as ORIGINAL_SPEC §6 states it.
///
/// `IF NOT EXISTS` throughout, so `open` is idempotent and needs no migration
/// machinery — this is a single table that has only ever had one shape.
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS commands (
  id          TEXT PRIMARY KEY,
  text        TEXT NOT NULL,
  status      TEXT NOT NULL,
  reply       TEXT,
  error       TEXT,
  created_at  INTEGER NOT NULL,
  started_at  INTEGER,
  finished_at INTEGER
);
CREATE INDEX IF NOT EXISTS commands_created_idx ON commands(created_at DESC);
CREATE TABLE IF NOT EXISTS settings (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
"#;

/// The lifecycle of one spoken turn.
///
/// The wire strings are `queued | running | done | timed_out | failed` and they
/// are the same five words in the database, the JSON API, and the console
/// (§10, "Copy"). `timed_out` has an underscore — `snake_case` on `TimedOut`
/// gives exactly that, and a test pins it, because `timedout` would be an
/// invisible break between the API and the page that renders it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Queued,
    Running,
    Done,
    TimedOut,
    Failed,
    /// Arrived while the kill switch was off: kept in the log, never typed into
    /// the pane. Terminal — flipping the switch back on does NOT replay held
    /// commands, because dumping a backlog of stale requests into a live
    /// session is worse than losing them.
    Held,
}

impl Status {
    /// The wire string. Kept hand-written rather than routed through serde so
    /// SQL binding needs no allocation and no round-trip through JSON.
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Queued => "queued",
            Status::Running => "running",
            Status::Done => "done",
            Status::TimedOut => "timed_out",
            Status::Failed => "failed",
            Status::Held => "held",
        }
    }

    /// Inverse of [`Status::as_str`]. `None` for anything else — a value in
    /// the database this binary does not know is a bug worth surfacing, not
    /// something to coerce into `failed`.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Status> {
        match s {
            "queued" => Some(Status::Queued),
            "running" => Some(Status::Running),
            "done" => Some(Status::Done),
            "timed_out" => Some(Status::TimedOut),
            "failed" => Some(Status::Failed),
            "held" => Some(Status::Held),
            _ => None,
        }
    }

    /// Is this an end state? `queued` and `running` are the two that a restart
    /// can strand; everything else is settled forever.
    pub fn is_terminal(&self) -> bool {
        // Held counts: nothing further will happen to it.
        matches!(self, Status::Done | Status::TimedOut | Status::Failed | Status::Held)
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl rusqlite::ToSql for Status {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        Ok(rusqlite::types::ToSqlOutput::from(self.as_str()))
    }
}

/// One row of `commands`.
///
/// `Serialize` because `/api/commands` hands this straight to the console; the
/// field names are the JSON keys and the console reads them by those names.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Command {
    pub id: String,
    pub text: String,
    pub status: Status,
    pub reply: Option<String>,
    pub error: Option<String>,
    /// Unix seconds, like every other `*_at` here.
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
}

/// A status string in the database that this binary has no variant for.
#[derive(Debug, thiserror::Error)]
#[error("commands.status = {0:?} is not one of queued|running|done|timed_out|failed")]
struct UnknownStatus(String);

/// The command log. Cheap to clone — the clone shares the connection.
#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

/// Every SELECT below asks for these columns in this order.
const COLUMNS: &str = "id, text, status, reply, error, created_at, started_at, finished_at";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Command> {
    let raw: String = r.get("status")?;
    let status = Status::from_str(&raw).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            2,
            rusqlite::types::Type::Text,
            Box::new(UnknownStatus(raw)),
        )
    })?;
    Ok(Command {
        id: r.get("id")?,
        text: r.get("text")?,
        status,
        reply: r.get("reply")?,
        error: r.get("error")?,
        created_at: r.get("created_at")?,
        started_at: r.get("started_at")?,
        finished_at: r.get("finished_at")?,
    })
}

/// Pragmas and schema, applied to a freshly opened connection.
fn prepare(conn: &Connection) -> anyhow::Result<()> {
    // SQLite defaults this OFF. There are no foreign keys in this schema yet,
    // but the spec asks for it and a future table that references `commands`
    // should not have to remember to turn it on.
    conn.pragma_update(None, "foreign_keys", "ON").context("PRAGMA foreign_keys")?;
    // WAL so the worker writing a reply never blocks the console reading the
    // list. (An in-memory database silently stays in "memory" mode; that is
    // fine and not worth failing over.)
    conn.pragma_update(None, "journal_mode", "WAL").context("PRAGMA journal_mode")?;
    conn.pragma_update(None, "busy_timeout", 5000).context("PRAGMA busy_timeout")?;
    conn.execute_batch(SCHEMA).context("applying the commands schema")?;
    Ok(())
}

impl Db {
    /// Open (creating if needed) the database at `path`, including its parent
    /// directory, and apply the schema.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let conn =
            Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        prepare(&conn).with_context(|| format!("preparing {}", path.display()))?;
        Ok(Db { conn: Arc::new(Mutex::new(conn)) })
    }

    /// A private, empty database that never touches the disk. Tests only.
    pub fn open_in_memory() -> anyhow::Result<Self> {
        let conn = Connection::open_in_memory().context("opening an in-memory database")?;
        prepare(&conn).context("preparing the in-memory database")?;
        Ok(Db { conn: Arc::new(Mutex::new(conn)) })
    }

    /// A poisoned lock means some other thread panicked *while holding* the
    /// connection. The connection itself is still usable and this daemon's job
    /// is to keep answering, so the poison is stepped over rather than
    /// propagated into every caller as an error they cannot act on.
    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Record a newly accepted command as `queued`.
    ///
    /// This is what `run_agent` does before returning (§3), so it must be a
    /// single fast statement. A duplicate `id` is an error rather than an
    /// overwrite: ids are four characters (§6), collisions are possible, and
    /// clobbering a live turn's row would lose its reply. The caller retries
    /// with a fresh id.
    pub fn insert(&self, id: &str, text: &str, now: i64) -> anyhow::Result<()> {
        self.conn()
            .execute(
                "INSERT INTO commands (id, text, status, created_at)
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![id, text, Status::Queued, now],
            )
            .with_context(|| format!("queueing command {id}"))?;
        Ok(())
    }

    /// The worker has the pane and is about to type this command in.
    ///
    /// Only a `queued` row moves — re-running this against a row that has
    /// already finished (or been settled by [`Db::reconcile_stranded`]) would
    /// otherwise resurrect it as `running` and leave the console showing a
    /// turn nothing will ever complete.
    pub fn mark_running(&self, id: &str, now: i64) -> anyhow::Result<()> {
        let n = self
            .conn()
            .execute(
                "UPDATE commands SET status = ?2, started_at = ?3
                  WHERE id = ?1 AND status = 'queued'",
                rusqlite::params![id, Status::Running, now],
            )
            .with_context(|| format!("starting command {id}"))?;
        if n == 0 {
            tracing::warn!("db: {id} was not queued when the worker picked it up; not started");
        }
        Ok(())
    }

    /// Settle a command in a terminal state (Part II §II.7).
    ///
    /// `reply` and `error` are written as given, `NULL` included, so a retry
    /// path cannot leave a stale error next to a good reply.
    pub fn finish(
        &self,
        id: &str,
        status: Status,
        reply: Option<&str>,
        error: Option<&str>,
        now: i64,
    ) -> anyhow::Result<()> {
        if !status.is_terminal() {
            tracing::warn!("db: finish({id}) called with the non-terminal status {status}");
        }
        let n = self
            .conn()
            .execute(
                "UPDATE commands SET status = ?2, reply = ?3, error = ?4, finished_at = ?5
                  WHERE id = ?1",
                rusqlite::params![id, status, reply, error, now],
            )
            .with_context(|| format!("finishing command {id}"))?;
        if n == 0 {
            tracing::warn!("db: finish({id}) matched no row — the outcome {status} was lost");
        }
        Ok(())
    }

    /// One command by id, or `None` if there is no such row.
    pub fn get(&self, id: &str) -> anyhow::Result<Option<Command>> {
        let sql = format!("SELECT {COLUMNS} FROM commands WHERE id = ?1");
        let got = self
            .conn()
            .query_row(&sql, rusqlite::params![id], row)
            .optional()
            .with_context(|| format!("reading command {id}"))?;
        Ok(got)
    }

    /// The console's list: newest first, capped at `limit`.
    ///
    /// `rowid DESC` breaks ties. `created_at` has one-second resolution and
    /// two commands spoken in the same second are entirely ordinary; without
    /// the tiebreak their order on the page would be whatever the query
    /// planner felt like that day.
    pub fn recent(&self, limit: usize) -> anyhow::Result<Vec<Command>> {
        let sql = format!(
            "SELECT {COLUMNS} FROM commands ORDER BY created_at DESC, rowid DESC LIMIT ?1"
        );
        let conn = self.conn();
        let mut q = conn.prepare(&sql).context("preparing the recent-commands query")?;
        let rows = q
            .query_map(rusqlite::params![i64::try_from(limit).unwrap_or(i64::MAX)], row)
            .context("listing recent commands")?;
        let out = rows.collect::<rusqlite::Result<Vec<_>>>().context("reading recent commands")?;
        Ok(out)
    }

    /// Everything still `queued`, oldest first — the worker's refill on boot.
    ///
    /// FIFO is the whole contract of the queue (§3), so this is `ASC`, with
    /// `rowid ASC` as the same-second tiebreak that keeps two commands spoken
    /// back to back in the order they were spoken.
    ///
    /// In practice [`Db::reconcile_stranded`] runs first at startup and empties
    /// this, so it returns rows only for a caller that refills mid-run.
    pub fn pending(&self) -> anyhow::Result<Vec<Command>> {
        let sql = format!(
            "SELECT {COLUMNS} FROM commands
              WHERE status = 'queued'
              ORDER BY created_at ASC, rowid ASC"
        );
        let conn = self.conn();
        let mut q = conn.prepare(&sql).context("preparing the pending-commands query")?;
        let rows = q.query_map([], row).context("listing pending commands")?;
        let out = rows.collect::<rusqlite::Result<Vec<_>>>().context("reading pending commands")?;
        Ok(out)
    }

    /// Fail every command a previous process left in flight, and say how many.
    ///
    /// The queue lives in memory and the worker owns exactly one turn at a
    /// time, so a restart — a deploy, a crash, a reboot — orphans whatever was
    /// `queued` or `running`. Nothing will ever finish those rows, and the
    /// console would show them spinning forever (§6).
    ///
    /// Both states are failed rather than re-queued. A `running` command
    /// probably reached the pane and may well have been answered, but the
    /// reply is unrecoverable; a `queued` one could in principle be re-sent,
    /// but silently re-typing a command into a tool-capable agent session
    /// minutes or hours later — after the person has moved on — is a worse
    /// surprise than an honest failure.
    ///
    /// Called once at startup, before the listener binds, so no request can
    /// observe a stranded row. Idempotent: a second call settles nothing.
    pub fn reconcile_stranded(&self, now: i64) -> anyhow::Result<usize> {
        let n = self
            .conn()
            .execute(
                "UPDATE commands
                    SET status = ?1, error = ?2, finished_at = ?3
                  WHERE status IN ('queued', 'running')",
                rusqlite::params![Status::Failed, STRANDED_ERROR, now],
            )
            .context("settling commands stranded by a restart")?;
        Ok(n)
    }
}

/// Seconds since the Unix epoch — the unit every `*_at` column uses.
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}


/// The kill switch.
///
/// Injection state lives in the database rather than in memory so that flipping
/// the switch off survives a restart. The failure everyone would regret is the
/// opposite default: a daemon that quietly resumes typing into a live terminal
/// because it was restarted while held.
///
/// Absent row means ON — a fresh install types, which is what the thing is for.
impl Db {
    pub fn injecting(&self) -> anyhow::Result<bool> {
        let conn = self.conn();
        let raw: Option<String> = conn
            .query_row(
                "SELECT value FROM settings WHERE key = 'injecting'",
                [],
                |r| r.get(0),
            )
            .optional()
            .context("read injecting")?;
        Ok(raw.map(|v| v == "1").unwrap_or(true))
    }

    pub fn set_injecting(&self, enabled: bool) -> anyhow::Result<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO settings (key, value) VALUES ('injecting', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![if enabled { "1" } else { "0" }],
        )
        .context("write injecting")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Db {
        Db::open_in_memory().unwrap()
    }

    /// The five words are a contract shared by the database, `/api/commands`
    /// and the console. `timed_out` is the one that would break silently.
    #[test]
    fn status_wire_strings_round_trip() {
        let all = [
            (Status::Queued, "queued"),
            (Status::Running, "running"),
            (Status::Done, "done"),
            (Status::TimedOut, "timed_out"),
            (Status::Failed, "failed"),
        ];
        for (s, want) in all {
            assert_eq!(s.as_str(), want);
            assert_eq!(Status::from_str(want), Some(s));
            assert_eq!(
                serde_json::to_string(&s).unwrap(),
                format!("\"{want}\""),
                "serde must agree with as_str; the console reads the JSON"
            );
            assert_eq!(
                serde_json::from_str::<Status>(&format!("\"{want}\"")).unwrap(),
                s
            );
        }
        assert_eq!(Status::TimedOut.as_str(), "timed_out", "not `timedout`");
        assert_eq!(Status::from_str("timedout"), None);
        assert_eq!(Status::from_str("timed out"), None);
        assert_eq!(Status::from_str("TIMED_OUT"), None);
        assert_eq!(Status::from_str(""), None);
    }

    #[test]
    fn terminal_states_are_the_three_that_a_restart_cannot_strand() {
        assert!(!Status::Queued.is_terminal());
        assert!(!Status::Running.is_terminal());
        for s in [Status::Done, Status::TimedOut, Status::Failed] {
            assert!(s.is_terminal(), "{s} should be terminal");
        }
    }

    #[test]
    fn insert_starts_queued_with_no_times_but_created_at() {
        let db = db();
        db.insert("a1b2", "remind me to email Dana", 1000).unwrap();
        let c = db.get("a1b2").unwrap().expect("the row we just wrote");
        assert_eq!(c.id, "a1b2");
        assert_eq!(c.text, "remind me to email Dana");
        assert_eq!(c.status, Status::Queued);
        assert_eq!(c.created_at, 1000);
        assert_eq!(c.reply, None);
        assert_eq!(c.error, None);
        assert_eq!(c.started_at, None, "nothing has started yet");
        assert_eq!(c.finished_at, None);
    }

    #[test]
    fn get_of_an_unknown_id_is_none_not_an_error() {
        assert!(db().get("zzzz").unwrap().is_none());
    }

    #[test]
    fn a_duplicate_id_is_refused_rather_than_clobbering_a_live_turn() {
        let db = db();
        db.insert("a1b2", "first", 1000).unwrap();
        assert!(db.insert("a1b2", "second", 1001).is_err());
        assert_eq!(db.get("a1b2").unwrap().unwrap().text, "first");
    }

    /// The full journey, once per terminal state (Part II §II.7).
    #[test]
    fn round_trip_to_each_terminal_status() {
        let cases = [
            (Status::Done, Some("Added to your reminders for 3pm today."), None),
            (Status::TimedOut, None, None),
            (Status::Failed, None, Some("no window named \"index MASTER\"")),
        ];
        for (i, (status, reply, error)) in cases.into_iter().enumerate() {
            let db = db();
            let id = format!("c{i}");

            db.insert(&id, "what's on my calendar tomorrow", 100).unwrap();
            assert_eq!(db.get(&id).unwrap().unwrap().status, Status::Queued);

            db.mark_running(&id, 101).unwrap();
            let running = db.get(&id).unwrap().unwrap();
            assert_eq!(running.status, Status::Running);
            assert_eq!(running.started_at, Some(101));
            assert_eq!(running.finished_at, None);

            db.finish(&id, status, reply, error, 142).unwrap();
            let done = db.get(&id).unwrap().unwrap();
            assert_eq!(done.status, status);
            assert_eq!(done.reply.as_deref(), reply);
            assert_eq!(done.error.as_deref(), error);
            assert_eq!(done.finished_at, Some(142));
            assert_eq!(done.started_at, Some(101), "finishing must not lose the start");
            assert_eq!(done.text, "what's on my calendar tomorrow", "the transcript survives");
        }
    }

    #[test]
    fn finish_clears_a_stale_error_when_it_writes_a_reply() {
        let db = db();
        db.insert("a1b2", "x", 1).unwrap();
        db.mark_running("a1b2", 2).unwrap();
        db.finish("a1b2", Status::Failed, None, Some("tmux went away"), 3).unwrap();
        db.finish("a1b2", Status::Done, Some("the answer"), None, 4).unwrap();
        let c = db.get("a1b2").unwrap().unwrap();
        assert_eq!(c.status, Status::Done);
        assert_eq!(c.reply.as_deref(), Some("the answer"));
        assert_eq!(c.error, None, "a good reply must not sit next to the old error");
    }

    /// A row that is no longer `queued` must not be dragged back into flight.
    #[test]
    fn mark_running_only_moves_a_queued_row() {
        let db = db();
        db.insert("a1b2", "x", 1).unwrap();
        db.finish("a1b2", Status::Done, Some("answered"), None, 2).unwrap();
        db.mark_running("a1b2", 3).unwrap();
        let c = db.get("a1b2").unwrap().unwrap();
        assert_eq!(c.status, Status::Done, "a finished command stays finished");
        assert_eq!(c.started_at, None);

        // An id that does not exist at all is a no-op, not an error.
        db.mark_running("zzzz", 4).unwrap();
    }

    #[test]
    fn recent_is_newest_first_and_honours_the_limit() {
        let db = db();
        for (id, at) in [("aaaa", 10), ("bbbb", 30), ("cccc", 20)] {
            db.insert(id, id, at).unwrap();
        }
        let ids: Vec<String> = db.recent(10).unwrap().into_iter().map(|c| c.id).collect();
        assert_eq!(ids, vec!["bbbb", "cccc", "aaaa"], "created_at DESC");

        let ids: Vec<String> = db.recent(2).unwrap().into_iter().map(|c| c.id).collect();
        assert_eq!(ids, vec!["bbbb", "cccc"], "the limit takes the newest, not the oldest");

        assert!(db.recent(0).unwrap().is_empty());
    }

    /// Two commands spoken inside the same second still render in the order
    /// they were spoken.
    #[test]
    fn recent_breaks_same_second_ties_by_insertion_order() {
        let db = db();
        for id in ["aaaa", "bbbb", "cccc"] {
            db.insert(id, id, 500).unwrap();
        }
        let ids: Vec<String> = db.recent(10).unwrap().into_iter().map(|c| c.id).collect();
        assert_eq!(ids, vec!["cccc", "bbbb", "aaaa"], "newest insert first");
    }

    #[test]
    fn recent_on_an_empty_log_is_empty() {
        assert!(db().recent(50).unwrap().is_empty());
    }

    #[test]
    fn pending_is_queued_only_oldest_first() {
        let db = db();
        db.insert("aaaa", "oldest", 10).unwrap();
        db.insert("bbbb", "newest", 30).unwrap();
        db.insert("cccc", "middle", 20).unwrap();
        db.insert("dddd", "in flight", 40).unwrap();
        db.insert("eeee", "answered", 50).unwrap();
        db.mark_running("dddd", 41).unwrap();
        db.finish("eeee", Status::Done, Some("yes"), None, 51).unwrap();

        let ids: Vec<String> = db.pending().unwrap().into_iter().map(|c| c.id).collect();
        assert_eq!(
            ids,
            vec!["aaaa", "cccc", "bbbb"],
            "FIFO, and neither the running nor the finished row"
        );
    }

    #[test]
    fn reconcile_stranded_settles_exactly_the_in_flight_rows() {
        let db = db();
        db.insert("qqqq", "never started", 10).unwrap();
        db.insert("rrrr", "cut off mid-turn", 20).unwrap();
        db.mark_running("rrrr", 21).unwrap();
        db.insert("dddd", "already answered", 30).unwrap();
        db.finish("dddd", Status::Done, Some("the answer"), None, 31).unwrap();
        db.insert("ffff", "already failed", 40).unwrap();
        db.finish("ffff", Status::Failed, None, Some("tmux went away"), 41).unwrap();
        db.insert("tttt", "already timed out", 50).unwrap();
        db.finish("tttt", Status::TimedOut, None, None, 52).unwrap();

        assert_eq!(db.reconcile_stranded(900).unwrap(), 2, "the queued and running rows only");

        for id in ["qqqq", "rrrr"] {
            let c = db.get(id).unwrap().unwrap();
            assert_eq!(c.status, Status::Failed, "{id}");
            assert_eq!(c.error.as_deref(), Some(STRANDED_ERROR), "{id}");
            assert_eq!(c.finished_at, Some(900), "a settled row must have an end time");
            assert_eq!(c.reply, None, "{id}");
        }

        // The three already-terminal rows are untouched, errors and replies included.
        let done = db.get("dddd").unwrap().unwrap();
        assert_eq!(done.status, Status::Done);
        assert_eq!(done.reply.as_deref(), Some("the answer"));
        assert_eq!(done.error, None);
        assert_eq!(done.finished_at, Some(31));

        let failed = db.get("ffff").unwrap().unwrap();
        assert_eq!(failed.status, Status::Failed);
        assert_eq!(failed.error.as_deref(), Some("tmux went away"), "its own error survives");
        assert_eq!(failed.finished_at, Some(41));

        let timed = db.get("tttt").unwrap().unwrap();
        assert_eq!(timed.status, Status::TimedOut);
        assert_eq!(timed.finished_at, Some(52));

        // Idempotent: the next boot has nothing left to settle.
        assert_eq!(db.reconcile_stranded(901).unwrap(), 0);
        assert!(db.pending().unwrap().is_empty(), "and the queue is empty afterwards");
    }

    #[test]
    fn reconcile_stranded_on_an_empty_log_settles_nothing() {
        assert_eq!(db().reconcile_stranded(1).unwrap(), 0);
    }

    #[test]
    fn open_creates_missing_parent_directories_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/deeper/indexd.db");

        let db = Db::open(&path).unwrap();
        db.insert("a1b2", "survives a restart", 7).unwrap();
        db.finish("a1b2", Status::Done, Some("kept"), None, 8).unwrap();
        drop(db);

        assert!(path.exists(), "{} should exist", path.display());

        // Reopening applies the schema again without complaint and the row is
        // still there — this is what a daemon restart does.
        let db = Db::open(&path).unwrap();
        let c = db.get("a1b2").unwrap().unwrap();
        assert_eq!(c.status, Status::Done);
        assert_eq!(c.reply.as_deref(), Some("kept"));
    }

    #[test]
    fn a_file_database_runs_in_wal_with_foreign_keys_on() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("indexd.db")).unwrap();
        let conn = db.conn();
        let mode: String =
            conn.query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
        assert_eq!(mode.to_ascii_lowercase(), "wal");
        let fk: i64 = conn.query_row("PRAGMA foreign_keys", [], |r| r.get(0)).unwrap();
        assert_eq!(fk, 1);
    }

    #[test]
    fn the_schema_matches_the_spec() {
        let db = db();
        let conn = db.conn();
        let cols: Vec<(String, String, i64)> = conn
            .prepare("PRAGMA table_info(commands)")
            .unwrap()
            .query_map([], |r| Ok((r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        let got: Vec<(&str, &str, bool)> =
            cols.iter().map(|(n, t, nn)| (n.as_str(), t.as_str(), *nn == 1)).collect();
        assert_eq!(
            got,
            vec![
                ("id", "TEXT", false),
                ("text", "TEXT", true),
                ("status", "TEXT", true),
                ("reply", "TEXT", false),
                ("error", "TEXT", false),
                ("created_at", "INTEGER", true),
                ("started_at", "INTEGER", false),
                ("finished_at", "INTEGER", false),
            ]
        );

        let index: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                  WHERE type = 'index' AND name = 'commands_created_idx'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(index, 1, "the console's list is ordered by created_at DESC");
    }

    /// A status this binary has no variant for must surface, not be guessed at.
    #[test]
    fn an_unknown_status_in_the_database_is_an_error_not_a_guess() {
        let db = db();
        db.insert("a1b2", "x", 1).unwrap();
        db.conn()
            .execute("UPDATE commands SET status = 'wat' WHERE id = 'a1b2'", [])
            .unwrap();
        let e = db.get("a1b2").unwrap_err().to_string();
        assert!(e.contains("a1b2"), "the context should name the row: {e}");
    }

    /// The row shape the console's JavaScript reads.
    #[test]
    fn a_command_serializes_with_the_field_names_the_console_expects() {
        let db = db();
        db.insert("a1b2", "hello", 1000).unwrap();
        db.mark_running("a1b2", 1001).unwrap();
        db.finish("a1b2", Status::TimedOut, None, None, 1601).unwrap();
        let json = serde_json::to_value(db.get("a1b2").unwrap().unwrap()).unwrap();
        assert_eq!(json["id"], "a1b2");
        assert_eq!(json["text"], "hello");
        assert_eq!(json["status"], "timed_out");
        assert!(json["reply"].is_null());
        assert_eq!(json["created_at"], 1000);
        assert_eq!(json["started_at"], 1001);
        assert_eq!(json["finished_at"], 1601);
    }

    #[test]
    fn now_unix_is_a_plausible_wall_clock_second() {
        // Well after this was written and well before it stops mattering.
        let n = now_unix();
        assert!(n > 1_750_000_000, "{n}");
        assert!(n < 4_000_000_000, "{n}");
    }

    #[test]
    fn a_clone_shares_the_same_database() {
        let db = db();
        let other = db.clone();
        db.insert("a1b2", "written through one handle", 1).unwrap();
        assert!(other.get("a1b2").unwrap().is_some(), "a clone must not be a fresh database");
    }

    #[test]
    fn injection_defaults_to_on_when_no_row_exists() {
        let db = Db::open_in_memory().unwrap();
        // A fresh install types. The opposite default would mean a new daemon
        // silently ignores the ring until someone finds the switch.
        assert!(db.injecting().unwrap());
    }

    #[test]
    fn injection_state_round_trips_and_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("indexd.db");

        let db = Db::open(&path).unwrap();
        db.set_injecting(false).unwrap();
        assert!(!db.injecting().unwrap());
        drop(db);

        // The whole point of storing this in SQLite rather than memory: a
        // restart must not quietly resume typing into a live terminal.
        let reopened = Db::open(&path).unwrap();
        assert!(!reopened.injecting().unwrap(), "held must survive a restart");

        reopened.set_injecting(true).unwrap();
        assert!(reopened.injecting().unwrap());
    }

    #[test]
    fn held_is_a_terminal_status_with_the_wire_string_held() {
        assert_eq!(Status::Held.as_str(), "held");
        assert_eq!(Status::from_str("held"), Some(Status::Held));
        assert!(
            Status::Held.is_terminal(),
            "nothing further happens to a held command; the switch does not replay them"
        );
    }

    #[test]
    fn a_held_command_is_not_returned_as_pending_work() {
        let db = Db::open_in_memory().unwrap();
        db.insert("held", "not typed", 100).unwrap();
        db.finish("held", Status::Held, None, None, 100).unwrap();
        db.insert("live", "typed", 101).unwrap();

        let pending: Vec<String> = db.pending().unwrap().into_iter().map(|c| c.id).collect();
        assert_eq!(pending, vec!["live"], "a restart must not resurrect held commands");
    }

    #[test]
    fn reconcile_leaves_held_commands_alone() {
        let db = Db::open_in_memory().unwrap();
        db.insert("h", "held one", 100).unwrap();
        db.finish("h", Status::Held, None, None, 100).unwrap();

        db.reconcile_stranded(200).unwrap();
        assert_eq!(db.get("h").unwrap().unwrap().status, Status::Held);
    }
}
