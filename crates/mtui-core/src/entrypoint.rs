//! Headless single-command dispatch entrypoint (`mtui-mcp` / embedding).
//!
//! This is the glue between the **three distinct argparse layers** mtui carries
//! (the correction that shaped P5.10 — do not conflate them):
//!
//! 1. **App invocation** — the top-level `mtui`/`mtui-mcp` process arguments,
//!    [`Args`](crate::args::Args) (port of upstream `mtui.cli.args.get_parser`).
//!    The real binary parses these with `Args::parse`, which exits the process
//!    on `--help`/`--version`/error exactly like upstream. This module takes an
//!    *already-parsed* `&Args`, so Layer 1 is the caller's responsibility (the
//!    binary is Phase 6).
//! 2. **REPL commands** — the per-command parsers the [`engine`](crate::engine)
//!    synthesises from the [`Registry`], run inside the REPL `cmdloop` and reused
//!    as MCP tools (port of upstream `mtui.commands._command.Command.parse_args`
//!    on the no-exit `ArgumentParser`). These never exit the process; they return
//!    a typed [`EngineError`].
//! 3. **MCP tool schema** — `mtui-mcp` translating each command's parser into
//!    JSON parameters (Phase 7). Not touched here.
//!
//! The headless single-command driver dispatches exactly one Layer-2 command
//! against a session and yields a process [`ExitStatus`]: given the parsed
//! top-level `Args` and one command line, it resolves, parses, and runs a
//! single command with no interactive loop. It is the headless single-command
//! primitive for `mtui-mcp` (Phase 7) and embedding callers.
//!
//! It is **not** a CLI mode: upstream `mtui` — and the mtui `mtui` binary —
//! has only two surfaces, the interactive REPL and `mtui-mcp`, and neither takes
//! a positional command. The interactive binary seeds the session and enters the
//! REPL (`mtui-cli::seed_session` + `Repl`); it never calls `run_once`.
//!
//! ## Exit-code contract
//!
//! Upstream `run_mtui` collapses everything to `Literal[0, 1]`. mtui
//! **intentionally deviates**: it distinguishes clap/argparse's usage-error
//! convention (exit `2`) from a runtime failure (exit `1`), while keeping
//! `--help`/`--version` a success (exit `0`). See [`ExitStatus`].

/// The process exit status a single non-interactive command run yields.
///
/// mtui deviates from upstream's collapsed `Literal[0, 1]` to preserve the
/// argparse/clap distinction between a *usage* error and a *runtime* failure:
///
/// * [`Ok`](ExitStatus::Ok) → `0` — the command ran, or clap printed
///   `--help`/`--version` (a success in argparse terms).
/// * [`Failure`](ExitStatus::Failure) → `1` — a runtime failure: unknown
///   command, unbalanced quotes, or the command body erroring.
/// * [`Usage`](ExitStatus::Usage) → `2` — a genuine argument *usage* error
///   (clap/argparse's exit-2 convention).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitStatus {
    /// Success (or help/version output). Process exit code `0`.
    Ok,
    /// Runtime failure. Process exit code `1`.
    Failure,
    /// Argument usage error. Process exit code `2`.
    Usage,
}

impl ExitStatus {
    /// The numeric process exit code (`0`, `1`, or `2`).
    #[must_use]
    fn code(self) -> i32 {
        match self {
            ExitStatus::Ok => 0,
            ExitStatus::Failure => 1,
            ExitStatus::Usage => 2,
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
    }
}
