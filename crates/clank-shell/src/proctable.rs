//! The process table: clank's record of running and completed invocations.
//!
//! A *process* is a running invocation, distinct from a [`Manifest`](crate::manifest::Manifest)
//! (static metadata about a command). This is the README's "internal process table" — PID, PPID,
//! type tag, argv, status, start time.
//!
//! **Granularity (increment 2): one process per executed line.** clank's `Session::run_line` runs a
//! whole line through Brush as one unit and never sees individual commands (that would need a Brush
//! fork), so each executed line becomes one row. `argv` is the line split on whitespace — enough for
//! `ps` display, not shell-accurate tokenization.
//!
//! **Determinism / durability:** `next_pid` and `start_seq` are table-local counters advanced only
//! in [`ProcessTable::spawn`], called only from `run_line`, whose order the Golem oplog replays
//! verbatim. With no wall-clock/RNG/thread input, the table is a pure deterministic fold over the
//! replayed line history — it reconstructs itself on recovery and needs no separate snapshot.
//!
//! **Reaching the table from the `ps` builtin.** Brush's `SimpleCommand::execute` is an associated
//! fn that can't capture state, and Brush's `Shell` has no clank slot. So the table is per-`Session`
//! state, and `run_line` briefly *installs* it into a **thread-local** slot ([`install`]) for the
//! duration of one line; the `ps` builtin reads the installed table via [`active`]. The slot is
//! thread-local, not process-global, so it means "the table for the line executing on *this*
//! thread" — a `Session` runs its lines on one thread at a time, so parallel Sessions (e.g. native
//! tests) never collide, and a single line's `ps` always reads its own Session's table. The slot
//! holds no durable data (only a transient pointer to the session mid-line), so it's replay-safe.
//!
//! Assumption: a single synchronous line's builtin runs on the same thread that called `install`
//! (verified on the native multi-thread runtime; always true on single-threaded wasm). If a future
//! increment runs pipeline stages on other worker threads, `ps` in such a stage would need the slot
//! plumbed through Brush's `ShellExtensions` instead — out of scope while execution is synchronous.

use std::cell::RefCell;
use std::sync::{Arc, Mutex};

use crate::process::ProcessKind;

/// The synthetic shell-root PID (an init-like process). Not a stored row; `render_ps` synthesizes
/// it so `ps` is never empty and every spawned row has a parent.
pub const SHELL_ROOT_PID: u32 = 1;

/// The first PID handed out to a real (spawned) process. PID 1 is reserved for the root.
pub const FIRST_PID: u32 = 2;

/// Process state, the README's five. Only `R` (running) and `Z` (completed) are reachable in this
/// increment; `S`/`T`/`P` are defined for the model but unreachable until background/paused
/// execution exists (a later increment).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcState {
    /// Running / active.
    R,
    /// Sleeping / waiting on remote work.
    S,
    /// Suspended.
    T,
    /// Completed, not yet reaped.
    Z,
    /// Paused — awaiting authorization or `prompt-user` input.
    P,
}

impl ProcState {
    /// The single-letter code shown in `ps`'s STAT column.
    fn code(self) -> char {
        match self {
            ProcState::R => 'R',
            ProcState::S => 'S',
            ProcState::T => 'T',
            ProcState::Z => 'Z',
            ProcState::P => 'P',
        }
    }
}

/// One process-table row: a single invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcRow {
    pub pid: u32,
    pub ppid: u32,
    pub kind: ProcessKind,
    /// The command line as argv (whitespace-split; display-only).
    pub argv: Vec<String>,
    pub state: ProcState,
    /// Logical start ordinal (monotonic within the session), not wall-clock — keeps the table
    /// fully deterministic under replay.
    pub start: u64,
}

impl ProcRow {
    /// The command as a display string (argv joined by spaces).
    fn command(&self) -> String {
        self.argv.join(" ")
    }
}

/// `ps` output mode, from the invocation flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PsMode {
    /// `ps` — `PID STAT COMMAND`.
    Default,
    /// `ps aux` — the wide BSD format.
    Aux,
    /// `ps -ef` — the wide System V format.
    Ef,
}

/// clank's table of running/completed processes. Per-`Session` state.
#[derive(Clone, Debug)]
pub struct ProcessTable {
    rows: Vec<ProcRow>,
    next_pid: u32,
    start_seq: u64,
}

impl Default for ProcessTable {
    fn default() -> Self {
        Self {
            rows: Vec::new(),
            next_pid: FIRST_PID,
            start_seq: 0,
        }
    }
}

impl ProcessTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a new process in state `R`, parented to the shell root, and return its PID. PIDs are
    /// monotonic and never reused.
    pub fn spawn(&mut self, kind: ProcessKind, argv: Vec<String>) -> u32 {
        let pid = self.next_pid;
        self.next_pid += 1;
        let start = self.start_seq;
        self.start_seq += 1;
        self.rows.push(ProcRow {
            pid,
            ppid: SHELL_ROOT_PID,
            kind,
            argv,
            state: ProcState::R,
            start,
        });
        pid
    }

    /// Mark a process complete (`R → Z`). No-op if the PID is unknown or already terminal. Z rows
    /// are not reaped in this increment — they accumulate (reaping arrives with `wait`/`/proc`).
    pub fn complete(&mut self, pid: u32) {
        if let Some(row) = self.rows.iter_mut().find(|r| r.pid == pid) {
            if row.state == ProcState::R {
                row.state = ProcState::Z;
            }
        }
    }

    /// The rows, oldest first (for tests and rendering).
    pub fn rows(&self) -> &[ProcRow] {
        &self.rows
    }

    /// Render the table in the given `ps` mode, including the synthetic root row.
    pub fn render_ps(&self, mode: PsMode) -> String {
        match mode {
            PsMode::Default => self.render_default(),
            PsMode::Aux => self.render_aux(),
            PsMode::Ef => self.render_ef(),
        }
    }

    fn render_default(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("{:>5} {:<4} {}\n", "PID", "STAT", "COMMAND"));
        // Synthetic root.
        out.push_str(&format!("{:>5} {:<4} {}\n", SHELL_ROOT_PID, "S", "clank"));
        for r in &self.rows {
            out.push_str(&format!(
                "{:>5} {:<4} {}\n",
                r.pid,
                r.state.code(),
                r.command()
            ));
        }
        out
    }

    fn render_aux(&self) -> String {
        // %CPU/%MEM/VSZ/RSS/TTY are not available in WASM — shown as `-` (README).
        let mut out = String::new();
        out.push_str(&format!(
            "{:<6} {:>5} {:>4} {:>4} {:>6} {:>6} {:<4} {:<4} {:<5} {:<5} {}\n",
            "USER", "PID", "%CPU", "%MEM", "VSZ", "RSS", "TTY", "STAT", "START", "TIME", "COMMAND"
        ));
        out.push_str(&format!(
            "{:<6} {:>5} {:>4} {:>4} {:>6} {:>6} {:<4} {:<4} {:<5} {:<5} {}\n",
            "clank", SHELL_ROOT_PID, "-", "-", "-", "-", "-", "S", "0", "-", "clank"
        ));
        for r in &self.rows {
            out.push_str(&format!(
                "{:<6} {:>5} {:>4} {:>4} {:>6} {:>6} {:<4} {:<4} {:<5} {:<5} {}\n",
                "clank",
                r.pid,
                "-",
                "-",
                "-",
                "-",
                "-",
                r.state.code(),
                r.start,
                "-",
                r.command()
            ));
        }
        out
    }

    fn render_ef(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "{:<6} {:>5} {:>5} {:>2} {:<6} {:<4} {:<5} {}\n",
            "UID", "PID", "PPID", "C", "STIME", "TTY", "TIME", "CMD"
        ));
        out.push_str(&format!(
            "{:<6} {:>5} {:>5} {:>2} {:<6} {:<4} {:<5} {}\n",
            "clank", SHELL_ROOT_PID, 0, "-", "0", "-", "-", "clank"
        ));
        for r in &self.rows {
            out.push_str(&format!(
                "{:<6} {:>5} {:>5} {:>2} {:<6} {:<4} {:<5} {}\n",
                "clank",
                r.pid,
                r.ppid,
                "-",
                r.start,
                "-",
                "-",
                r.command()
            ));
        }
        out
    }
}

thread_local! {
    /// The transient "which session is mid-`run_line` on this thread" slot. Holds no durable data;
    /// populated by [`install`] for the duration of one line and read by the `ps` builtin via
    /// [`active`]. Thread-local so parallel Sessions (native tests) don't collide.
    static ACTIVE: RefCell<Option<Arc<Mutex<ProcessTable>>>> = const { RefCell::new(None) };
}

/// Install `table` as the active process table for the current line, returning an RAII guard that
/// restores the previous slot value on drop. Nesting is supported (the guard saves/restores),
/// though clank executes one line at a time so nesting doesn't occur in practice.
#[must_use]
pub fn install(table: Arc<Mutex<ProcessTable>>) -> InstallGuard {
    let previous = ACTIVE.with(|slot| slot.borrow_mut().replace(table));
    InstallGuard { previous }
}

/// The currently-installed process table, if a line is executing on this thread.
pub fn active() -> Option<Arc<Mutex<ProcessTable>>> {
    ACTIVE.with(|slot| slot.borrow().clone())
}

/// Restores the previous active-table slot when dropped.
pub struct InstallGuard {
    previous: Option<Arc<Mutex<ProcessTable>>>,
}

impl Drop for InstallGuard {
    fn drop(&mut self) {
        let previous = self.previous.take();
        ACTIVE.with(|slot| *slot.borrow_mut() = previous);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn spawn_allocates_monotonic_pids_from_first_pid() {
        let mut t = ProcessTable::new();
        let p1 = t.spawn(ProcessKind::Builtin, argv("echo a"));
        let p2 = t.spawn(ProcessKind::Builtin, argv("echo b"));
        let p3 = t.spawn(ProcessKind::Builtin, argv("ls"));
        assert_eq!((p1, p2, p3), (FIRST_PID, FIRST_PID + 1, FIRST_PID + 2));
        // All parented to the root, all born Running.
        for r in t.rows() {
            assert_eq!(r.ppid, SHELL_ROOT_PID);
            assert_eq!(r.state, ProcState::R);
        }
    }

    #[test]
    fn complete_flips_running_to_zombie() {
        let mut t = ProcessTable::new();
        let pid = t.spawn(ProcessKind::Builtin, argv("echo hi"));
        assert_eq!(t.rows()[0].state, ProcState::R);
        t.complete(pid);
        assert_eq!(t.rows()[0].state, ProcState::Z);
        // Idempotent / harmless on unknown pids.
        t.complete(pid);
        t.complete(9999);
        assert_eq!(t.rows()[0].state, ProcState::Z);
    }

    #[test]
    fn start_ordinals_are_monotonic() {
        let mut t = ProcessTable::new();
        t.spawn(ProcessKind::Builtin, argv("a"));
        t.spawn(ProcessKind::Builtin, argv("b"));
        assert_eq!(t.rows()[0].start, 0);
        assert_eq!(t.rows()[1].start, 1);
    }

    #[test]
    fn render_default_has_header_root_and_rows() {
        let mut t = ProcessTable::new();
        let pid = t.spawn(ProcessKind::Builtin, argv("echo hi"));
        t.complete(pid);
        let out = t.render_ps(PsMode::Default);
        assert!(out.contains("PID"));
        assert!(out.contains("STAT"));
        assert!(out.contains("COMMAND"));
        // Synthetic root.
        assert!(out.contains(" 1 "));
        assert!(out.contains("clank"));
        // The completed command shows as Z.
        let cmd_line = out.lines().find(|l| l.contains("echo hi")).unwrap();
        assert!(cmd_line.contains("Z"));
    }

    #[test]
    fn render_aux_shows_dashes_for_cpu_and_mem() {
        let mut t = ProcessTable::new();
        t.spawn(ProcessKind::Builtin, argv("ls"));
        let out = t.render_ps(PsMode::Aux);
        assert!(out.contains("%CPU"));
        assert!(out.contains("%MEM"));
        assert!(out.contains("USER"));
        // The row for `ls` has `-` in the cpu/mem columns.
        let row = out.lines().find(|l| l.contains("ls")).unwrap();
        assert!(row.contains('-'));
        assert!(row.contains("clank")); // USER
    }

    #[test]
    fn render_ef_shows_ppid_column() {
        let mut t = ProcessTable::new();
        t.spawn(ProcessKind::Builtin, argv("ls"));
        let out = t.render_ps(PsMode::Ef);
        assert!(out.contains("PPID"));
        assert!(out.contains("CMD"));
        // The spawned row's PPID is the shell root.
        let row = out.lines().find(|l| l.contains("ls")).unwrap();
        assert!(row.contains(&SHELL_ROOT_PID.to_string()));
    }

    /// The critical soundness proof: two tables are fully independent — building a second one never
    /// disturbs the first's PIDs/rows. (Guards against the process-global-table hazard.)
    #[test]
    fn two_tables_are_independent() {
        let mut a = ProcessTable::new();
        let mut b = ProcessTable::new();
        let a1 = a.spawn(ProcessKind::Builtin, argv("a1"));
        let b1 = b.spawn(ProcessKind::Builtin, argv("b1"));
        let a2 = a.spawn(ProcessKind::Builtin, argv("a2"));
        // Both start from FIRST_PID independently.
        assert_eq!(a1, FIRST_PID);
        assert_eq!(b1, FIRST_PID);
        assert_eq!(a2, FIRST_PID + 1);
        assert_eq!(a.rows().len(), 2);
        assert_eq!(b.rows().len(), 1);
    }

    /// Determinism: identical spawn sequences produce identical PIDs and ordinals — the property
    /// that makes the table a pure function of replayed history.
    #[test]
    fn identical_sequences_produce_identical_tables() {
        let build = || {
            let mut t = ProcessTable::new();
            t.spawn(ProcessKind::Builtin, argv("echo a"));
            t.spawn(ProcessKind::Builtin, argv("echo b"));
            t
        };
        assert_eq!(build().rows(), build().rows());
    }

    #[test]
    fn install_guard_installs_and_restores() {
        assert!(active().is_none());
        let table = Arc::new(Mutex::new(ProcessTable::new()));
        {
            let _g = install(table.clone());
            assert!(active().is_some());
            // The installed table is the one we passed.
            active()
                .unwrap()
                .lock()
                .unwrap()
                .spawn(ProcessKind::Builtin, argv("x"));
            assert_eq!(table.lock().unwrap().rows().len(), 1);
        }
        // Restored to empty after the guard drops.
        assert!(active().is_none());
    }
}
