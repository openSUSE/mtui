//! The process exit-code contract shared by mtui's two process entrypoints
//! (`mtui`, `mtui-mcp`).
//!
//! This distinguishes the **three distinct argparse layers** mtui carries —
//! do not conflate them:
//!
//! 1. **App invocation** — the top-level `mtui`/`mtui-mcp` process arguments,
//!    [`Args`](crate::args::Args). The real binary parses these with
//!    `Args::parse`, which exits the process on `--help`/`--version`/error, the
//!    standard clap convention.
//! 2. **REPL commands** — the per-command parsers the [`engine`](crate::engine)
//!    synthesises from the [`Registry`](crate::registry::Registry), run inside
//!    the REPL `cmdloop` and reused as MCP tools. These never exit the
//!    process; they return a typed
//!    [`EngineError`](crate::engine::EngineError).
//! 3. **MCP tool schema** — `mtui-mcp` translating each command's parser into
//!    JSON parameters. Not touched here.
//!
//! Neither entrypoint has a headless single-command CLI mode: the `mtui`
//! binary has only two surfaces, the interactive REPL and `mtui-mcp`, and
//! neither takes a positional command. The interactive binary seeds the
//! session and enters the REPL (`mtui-cli::seed_session` + `Repl`).
//!
//! ## Exit-code contract
//!
//! mtui distinguishes clap/argparse's usage-error convention (exit `2`) from a
//! runtime failure (exit `1`), while keeping `--help`/`--version` a success
//! (exit `0`). One status is not an argparse outcome at all: a REPL session
//! force-quit by a double Ctrl-C exits `130` (128 + `SIGINT`), the status the
//! process would have had when the signal still killed it outright. See
//! [`ExitStatus`], and `mtui_cli::ReplExit`, which maps onto it.
//!
//! This is the whole vocabulary. Anything downstream — packaging, wrapper
//! scripts, CI — can rely on `0`/`1`/`2`/`130` and nothing else.

/// A process exit status of either entrypoint.
///
/// Three of the four preserve the argparse/clap distinction between a *usage*
/// error and a *runtime* failure; the fourth is the signal convention:
///
/// * [`Ok`](ExitStatus::Ok) → `0` — the command ran, or clap printed
///   `--help`/`--version` (a success in argparse terms).
/// * [`Failure`](ExitStatus::Failure) → `1` — a runtime failure: unknown
///   command, unbalanced quotes, or the command body erroring.
/// * [`Usage`](ExitStatus::Usage) → `2` — a genuine argument *usage* error
///   (clap/argparse's exit-2 convention).
/// * [`Interrupted`](ExitStatus::Interrupted) → `130` — the REPL was
///   force-quit by a double Ctrl-C (128 + `SIGINT`); see `mtui_cli::ReplExit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitStatus {
    /// Success (or help/version output). Process exit code `0`.
    Ok,
    /// Runtime failure. Process exit code `1`.
    Failure,
    /// Argument usage error. Process exit code `2`.
    Usage,
    /// Force-quit by a double Ctrl-C. Process exit code `130`.
    ///
    /// Not an argparse outcome: it is 128 + `SIGINT`, so a caller that already
    /// reads "killed by signal N" as 128 + N sees what it expects — which is
    /// what the operator got before mtui handled the signal at all.
    Interrupted,
}

impl ExitStatus {
    /// The numeric process exit code (`0`, `1`, `2`, or `130`).
    #[must_use]
    fn code(self) -> i32 {
        match self {
            ExitStatus::Ok => 0,
            ExitStatus::Failure => 1,
            ExitStatus::Usage => 2,
            ExitStatus::Interrupted => 130,
        }
    }
}

impl From<ExitStatus> for i32 {
    fn from(status: ExitStatus) -> Self {
        status.code()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_status_maps_to_i32() {
        assert_eq!(i32::from(ExitStatus::Ok), 0);
        assert_eq!(i32::from(ExitStatus::Failure), 1);
        assert_eq!(i32::from(ExitStatus::Usage), 2);
        // 128 + SIGINT: what a Ctrl-C death reported before mtui handled the
        // signal, and what wrapper scripts already read as "interrupted".
        assert_eq!(i32::from(ExitStatus::Interrupted), 130);
    }
}
