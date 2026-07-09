# Plan — P6.6: prompter + notification (bead `mtui-rs-lhz.6`)

## Current state

Phase 6 (mtui-cli REPL) is 77% done; `prompter.rs` and `notification.rs` are the
two remaining Phase-6 files. Upstream has three cooperating pieces that mtui-rs
has **not** yet ported: `cli/prompter.py` (a `threading.Lock`-serialized stdin
reader), `support/spinner.py` (a `|/-\` TTY spinner + `spinner_suspended()` paint
lock that `ask()` holds during a read), and `cli/notification.py` (a best-effort
desktop toast, `notify_user` in `repl.py`). In mtui-rs the spinner is explicitly
**deferred** (`mtui-hosts/src/target/actions.rs:29`), the russh connection has
**no interactive command-timeout prompt** (it aborts with `HostError::Timeout` —
the upstream `interactive=False` branch, `ssh.rs:466-473`), and there is no
notification path. The `interactive` flag is already threaded through
`run_parallel`/`RunCommand` as a seam but renders nothing.

**Scope (confirmed with user): full parity** — Prompter + wire it into a new SSH
command-timeout prompt + un-defer the TTY spinner it suspends; notification via
feature-gated `notify-rust` with headless no-op guards.

## Design constraints (must hold)

- **No crate cycle.** `mtui-hosts` must not depend on `mtui-cli`. The Prompter,
  spinner, and timeout-prompt *behavior* live low (hosts/types), but the concrete
  stdin reader + spinner rendering are injected from `mtui-cli` via a trait/callable
  seam (mirror `timeout_prompt: Callable` in upstream `connection.py:80`).
- **Async, not threads.** Upstream serializes with `threading.Lock` because
  workers are threads; mtui-rs fans out with `tokio` tasks, so the Prompter uses a
  `tokio::sync::Mutex` and an async `ask()`. Never hold a std `Mutex` across
  `.await`.
- **Headless/MCP safety.** `mtui-mcp` and piped/CI runs have no TTY. `ask()` under
  `interactive=false` must **not** read stdin; the spinner and notifications must
  be strict no-ops off a TTY (upstream `_desktop_available`, `sys.stderr.isatty`).
- **Injectable reader for tests.** `Prompter::new(reader)` takes the reader so
  unit tests never touch a real terminal (upstream `Prompter.__init__(reader)`).
- Preserve the pubkey-only + stdin-detached contracts; add no password auth.

## Plan

### A. Spinner (un-defer) — `mtui-hosts`

1. [medium] Create `crates/mtui-hosts/src/target/spinner.rs`: an async TTY
   spinner + a process-wide paint coordinator (the `spinner_suspended` analogue).
   Port `support/spinner.py`: `TtySpinner` driven by a `tokio` task painting
   `\r[{frame}] {desc}` to **stderr** on a 100ms tick, enabled only when
   `stderr.is_terminal()` (`std::io::IsTerminal`); a global paint lock
   (`tokio::sync::Mutex` or `std::sync::Mutex` on a small guard) + an `_active`
   registry; a `suspend()` async guard that erases the frame (`\r\x1b[K`), holds
   the lock for its lifetime, and is a no-op when no spinner is active / off a TTY.
   — verify: unit tests assert (a) off-TTY start/stop is a no-op and never writes,
   (b) `is_stopped()` flips after `stop()` even off-TTY, (c) `suspend()` erases +
   blocks a concurrent paint (test via an injected writer sink, not real stderr).

2. [small] Wire the spinner into `run_parallel`/`RunCommand` (`actions.rs`):
   when `interactive` and `desc` is `Some`, start a `TtySpinner(desc)` for the
   duration of the fan-out; stop it in a `finally`-equivalent (guard/`Drop` or
   explicit stop on both success and early return). Remove the "Deferred: the TTY
   spinner" note. — verify: existing `actions` tests stay green; add one test that
   a fan-out with `interactive=false` starts no spinner (injected sink unchanged).

### B. Prompter — `mtui-hosts` (behavior) + injected reader

3. [medium] Create `crates/mtui-hosts/src/prompter.rs`: `Prompter` owning a
   `tokio::sync::Mutex<()>` and an injectable async reader
   (`Arc<dyn Fn(&str) -> BoxFuture<'static, io::Result<String>> + Send + Sync>`,
   or a small `AsyncPrompt` trait). `async fn ask(&self, text: &str)`: acquire the
   mutex, `spinner::suspend().await` for the read, call the reader, release in
   order (drop guards). Off-interactive callers simply don't construct/pass a
   Prompter (matches `timeout_prompt=None`). — verify: unit tests with a stub
   reader assert (a) `ask` returns the reader's value, (b) two concurrent `ask`
   calls serialize (reader observes non-overlapping entry via a shared counter),
   (c) `ask` suspends any active spinner (injected sink shows erase before read).

### C. SSH command-timeout prompt — `mtui-hosts` connection

4. [medium] Add the interactive command-timeout seam to the russh connection
   (`ssh.rs`, `connection/mod.rs`). Mirror upstream `connection.py`:
   a `timeout_prompt: Option<TimeoutPrompt>` (an async callable/trait returning
   the user's answer) + an `interactive: bool` on the connection builder. In the
   `run` loop timeout branch (`ssh.rs:466-473`): if `interactive` and a prompt is
   set, `ask("command '<cmd>' timed out; wait? [Y/n] ")`; empty/`y`/`Y` → continue
   the wait loop, `n`/`N` → abort with `HostError::Timeout`; if no prompt (headless
   / `interactive=false`) keep today's immediate `Timeout` abort **but** emit one
   `WARN` (upstream's "silence is observable" log). — verify: unit tests over a
   `MockConnection`/injected clock: (a) headless timeout still aborts + logs WARN,
   (b) prompt returning "" loops and then completes when data arrives, (c) prompt
   returning "n" aborts with `Timeout`. Gate any real-sshd path behind `#[ignore]`.

### D. Notification — `mtui-cli`, feature `notify`

5. [small] Create `crates/mtui-cli/src/notification.rs`: port `notification.py`.
   `pub fn display(summary: Option<&str>, text: Option<&str>, icon: Option<&str>)`.
   `desktop_available()`: `false` unless `stdin.is_terminal()`; on macOS always
   `true`; on Linux/BSD require `DISPLAY` or `WAYLAND_DISPLAY`. Behind
   `#[cfg(feature = "notify")]` use `notify-rust` (best-effort, `tracing::debug`
   on failure, never panic); without the feature the body is a compile-time no-op
   that still runs the guards + debug-logs "notifications disabled". — verify:
   unit tests assert `display` is a silent no-op when the guard env says headless
   (override via injected `is_tty`/`env` seam so the test is deterministic), and
   that the no-feature build compiles (feature-matrix).

6. [small] Add a `notify_user(msg, error: bool)` helper on the REPL/session
   display path (upstream `repl.py:notify_user`, `class_="stock_dialog-error"` →
   `dialog-error` icon) and call it from the `update` command's start
   ("updating <RRID> …") and finish ("updating <RRID> finished") toasts
   (upstream `commands/update.py:72,81`). Keep it a thin call into
   `notification::display` so headless is a no-op. — verify: a REPL/update unit
   test asserts the notify call is issued (inject a recording `display` fn) and is
   a no-op under headless.

### E. Cargo + wiring

7. [trivial] `Cargo.toml`: add `notify = ["notify-rust"]` feature +
   `notify-rust` optional dep to `crates/mtui-cli/Cargo.toml`; add `notify-rust`
   to the workspace deps table. Export `notification::display` from
   `mtui-cli/src/lib.rs`. Export `Prompter` + `spinner` from `mtui-hosts` `lib.rs`.
   — verify: `cargo build -p mtui-cli` and `--features notify` both compile;
   feature-matrix (`--no-default-features`, `--all-features`) green.

8. [small] Composition root wiring: in the `mtui-cli` startup/repl path, build a
   `Prompter` with a real stdin reader and thread it + `interactive=true` into the
   host connection factory / `Session` when running under the REPL; under
   `mtui-mcp` leave it unset (`interactive=false`) — upstream `mcp/session.py:325`
   sets `prompter = None`. — verify: REPL smoke path builds a Prompter; the MCP
   crate compiles unchanged (no Prompter, no cycle).

## Files

- **Create:**
  - `crates/mtui-hosts/src/target/spinner.rs`
  - `crates/mtui-hosts/src/prompter.rs`
  - `crates/mtui-cli/src/notification.rs`
  - tests as noted (colocated `#[cfg(test)]` + any `crates/*/tests/*` integration)
- **Modify:**
  - `crates/mtui-hosts/src/target/actions.rs` (spinner in fan-out; drop deferral note)
  - `crates/mtui-hosts/src/connection/ssh.rs`, `connection/mod.rs` (timeout-prompt seam)
  - `crates/mtui-hosts/src/lib.rs` / `target/mod.rs` (module exports)
  - `crates/mtui-cli/src/lib.rs` (export `notification`, `notify_user` hook)
  - `crates/mtui-cli/src/repl.rs` or `startup.rs` (build+thread Prompter; notify hook)
  - `crates/mtui-core/src/commands/update.rs` (start/finish notify calls)
  - `crates/mtui-cli/Cargo.toml`, root `Cargo.toml` (feature + `notify-rust`)
- **Delete:** none.

## Risks

- **Crate-cycle regression.** Putting the Prompter's *reader* or the spinner's
  *renderer* in `mtui-hosts` would be fine, but importing anything from `mtui-cli`
  into `mtui-hosts` creates a cycle. Mitigation: inject via trait/callable only;
  assert with `cargo build -p mtui-hosts` in isolation.
- **`await`-across-lock unsoundness.** The Prompter must use `tokio::sync::Mutex`
  (not `std::Mutex`) since `ask` awaits the reader while holding it; a std mutex
  across `.await` is a clippy `-D warnings` failure and a real hazard.
- **Spinner ↔ stderr under async fan-out.** Multiple tasks + a paint task race on
  stderr. The paint lock + `suspend()` must cover every non-spinner stderr write
  during a spin, or frames interleave with output. Test via an injected sink, not
  the real terminal, so CI stays deterministic and offline.
- **SSH timeout-prompt scope creep.** The `run` loop's timeout is a *no-output*
  window, not a wall-clock command timeout; "wait?" must resume the same wait
  loop, not restart the command. Keep the branch minimal and mirror upstream's
  Enter/Y-default exactly.
- **notify-rust platform surface.** `notify-rust` pulls DBus (Linux) / NSUserom
  paths (mac); keep it optional + best-effort so a build without the feature (and
  a headless run with it) never breaks.

## Alternatives considered

- **Minimal prompter only (no SSH prompt, no spinner).** Rejected per user choice
  — they asked for full parity. It would have been the smallest viable diff
  (single `prompter.rs` + unit tests) but leaves the Prompter with no real
  consumer and the DoD's spinner-suspension semantics untestable end-to-end.
- **notification.rs stub with no dependency.** Rejected per user choice; would not
  satisfy the DoD "optional desktop notification (feature `notify`)" item.
- **std `threading`-style lock (1:1 port).** Rejected: the fan-out is `tokio`
  tasks, not OS threads; a std `Mutex` held across the async read is unsound and
  clippy-hostile. The `tokio::sync::Mutex` is the idiomatic successor.

## Success criteria

- [ ] `cargo fmt --all --check` clean.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean (incl. no
      `await_holding_lock`).
- [ ] `cargo test --workspace` green, incl. new unit tests for spinner
      (off-TTY no-op, suspend erases), Prompter (serialization + spinner-suspend +
      injected reader), SSH timeout prompt (headless-abort+WARN / wait / abort),
      notification (headless no-op), and update start/finish notify calls.
- [ ] Feature matrix green: `cargo build --workspace --no-default-features` **and**
      `--all-features` **and** `cargo build -p mtui-cli --features notify`.
- [ ] `cargo build -p mtui-hosts` in isolation compiles (no cycle into mtui-cli).
- [ ] `mtui-mcp` builds unchanged with `interactive=false` / no Prompter.
- [ ] >=80% patch coverage on new/changed lines.

## Handoff

Read-only plan produced in **plan mode**. To implement, switch to **build mode**
and run `br update mtui-rs-lhz.6 --status=in_progress`, then execute steps A→E.
Follow-up beads to file if descoped later: none (full-parity path chosen).
