//! Command-log storage, ported from `mtui/types/{commandlog,hostlog}.py`.
//!
//! A [`CommandLog`] records one command executed on a target host and its
//! outcome; a [`HostLog`] is an ordered list of them.
//!
//! ## Deviations from upstream
//!
//! Upstream's `HostLog` subclasses `list` and overrides `append`/`insert` with
//! runtime `*args` unpacking, length checks (`"it need 5 args"`), and
//! `str | bytes` coercion via `to_string`. In Rust the type system makes all of
//! that unnecessary: [`push`](HostLog::push) takes a fully-typed
//! [`CommandLog`], so the arity and type errors upstream
//! guards against at runtime simply cannot occur. Read access is provided via
//! [`Deref`] to a `[CommandLog]` slice, giving iteration, indexing, `len`, etc.
//! for free.

use std::ops::Deref;

/// A single command-execution log entry.
///
/// Ported from the upstream `CommandLog` `NamedTuple`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CommandLog {
    /// The command that was run.
    pub command: String,
    /// The command's standard output.
    pub stdout: String,
    /// The command's standard error.
    pub stderr: String,
    /// The command's exit code.
    ///
    /// A genuine POSIX exit code is 0–255; negative values are sentinels for
    /// "no exit code" (e.g. the command was killed by a signal or timed out),
    /// mirroring upstream mtui's use of `-1`. `i16` covers both ranges without
    /// the width of a C `int`.
    pub exitcode: i16,
    /// The command's runtime, in seconds.
    pub runtime: i64,
    /// Whether the captured `stdout`/`stderr` was truncated because the command
    /// exceeded the connection layer's output byte caps.
    ///
    /// `false` for every non-SSH producer (mocks, synthesised logs);
    /// only [`mtui-hosts`](../../mtui_hosts)' bounded capture sets it. When
    /// `true`, `stdout`/`stderr` hold the capped prefix and the overflow was
    /// discarded rather than buffered.
    pub truncated: bool,
    /// Whether the command was aborted because it hit the connection layer's
    /// absolute execution deadline (as opposed to completing on its own).
    ///
    /// `false` for every non-SSH producer; only the SSH `run` path sets it.
    pub timed_out: bool,
}

impl CommandLog {
    /// Creates a new [`CommandLog`] with the truncation/timeout flags cleared.
    ///
    /// This is the primary constructor used by every producer that cannot
    /// truncate or deadline output (mocks, synthesised logs). The SSH
    /// connection layer, which *can*, uses [`with_flags`](Self::with_flags) to
    /// record those conditions.
    #[must_use]
    pub fn new(
        command: impl Into<String>,
        stdout: impl Into<String>,
        stderr: impl Into<String>,
        exitcode: i16,
        runtime: i64,
    ) -> Self {
        Self {
            command: command.into(),
            stdout: stdout.into(),
            stderr: stderr.into(),
            exitcode,
            runtime,
            truncated: false,
            timed_out: false,
        }
    }

    /// Sets the [`truncated`](Self::truncated) / [`timed_out`](Self::timed_out)
    /// flags, returning the updated log.
    ///
    /// Used by the SSH `run` path to record that captured output was capped or
    /// that the command was aborted at its absolute deadline, without changing
    /// [`new`](Self::new)'s signature for the many callers that never do either.
    #[must_use]
    pub fn with_flags(mut self, truncated: bool, timed_out: bool) -> Self {
        self.truncated = truncated;
        self.timed_out = timed_out;
        self
    }
}

/// An ordered list of [`CommandLog`] entries for a single host.
///
/// Ported from upstream `HostLog(list[CommandLog])`. Dereferences to a
/// `[CommandLog]` slice for read access (iteration, indexing, `len`, `iter`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostLog {
    entries: Vec<CommandLog>,
}

impl HostLog {
    /// Creates an empty [`HostLog`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a command-log entry.
    pub fn push(&mut self, entry: CommandLog) {
        self.entries.push(entry);
    }
}

impl Deref for HostLog {
    type Target = [CommandLog];

    fn deref(&self) -> &Self::Target {
        &self.entries
    }
}

impl FromIterator<CommandLog> for HostLog {
    fn from_iter<I: IntoIterator<Item = CommandLog>>(iter: I) -> Self {
        Self {
            entries: iter.into_iter().collect(),
        }
    }
}

impl IntoIterator for HostLog {
    type Item = CommandLog;
    type IntoIter = std::vec::IntoIter<CommandLog>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.into_iter()
    }
}

impl<'a> IntoIterator for &'a HostLog {
    type Item = &'a CommandLog;
    type IntoIter = std::slice::Iter<'a, CommandLog>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(cmd: &str, code: i16) -> CommandLog {
        CommandLog::new(cmd, "out", "err", code, 1)
    }

    #[test]
    fn command_log_fields() {
        let c = CommandLog::new("ls", "a\nb", "", 0, 2);
        assert_eq!(c.command, "ls");
        assert_eq!(c.stdout, "a\nb");
        assert_eq!(c.stderr, "");
        assert_eq!(c.exitcode, 0);
        assert_eq!(c.runtime, 2);
    }

    #[test]
    fn command_log_new_clears_flags() {
        let c = CommandLog::new("ls", "", "", 0, 0);
        assert!(!c.truncated);
        assert!(!c.timed_out);
    }

    #[test]
    fn command_log_with_flags_sets_both() {
        let c = CommandLog::new("ls", "x", "", 0, 1).with_flags(true, true);
        assert!(c.truncated);
        assert!(c.timed_out);
        // Other fields are preserved.
        assert_eq!(c.stdout, "x");
        assert_eq!(c.exitcode, 0);
    }

    #[test]
    fn push_and_len() {
        let mut log = HostLog::new();
        assert!(log.is_empty());
        log.push(entry("a", 0));
        log.push(entry("b", 1));
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].command, "a");
        assert_eq!(log[1].exitcode, 1);
    }

    #[test]
    fn iteration_by_ref_and_value() {
        let log: HostLog = [entry("a", 0), entry("b", 0)].into_iter().collect();
        let by_ref: Vec<&str> = (&log).into_iter().map(|e| e.command.as_str()).collect();
        assert_eq!(by_ref, ["a", "b"]);
        let by_val: Vec<String> = log.into_iter().map(|e| e.command).collect();
        assert_eq!(by_val, ["a", "b"]);
    }
}
