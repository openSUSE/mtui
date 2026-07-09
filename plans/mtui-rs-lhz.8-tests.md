# Plan — mtui-rs-lhz.8 (P6.8): tests — coverage gap-fill + REPL EOF smoke

> **Task:** "Tests: completion/history/highlighter units, REPL smoke,
> non-interactive e2e." Phase 6 (mtui-cli REPL + mtui binary), P2, source
> PLAN-phase6.md task 8. Last open child of the Phase 6 epic (8/10 closed).
> **Depends on:** P6.1–P6.7 — all CLOSED.
> **Scope decision (confirmed with user):** *Gap-fill + EOF smoke.* Add tests
> only where coverage is genuinely thin (`repl.rs`, `shell.rs`,
> `notification.rs`) plus a piped-stdin/EOF-exits-0 binary smoke test. Record in
> the task that the named unit coverage already landed with P6.3–P6.7 and that
> **no non-interactive single-command CLI mode exists** to e2e-test.

## Current state

The unit tests the task title names already exist and are strong — they landed
incrementally with P6.3–P6.7. `cargo test -p mtui-cli` = **58 green** (53 unit +
5 smoke). Per-module region/line coverage: completer 95.7%/91.9%, highlighter
97.4%/95.0%, history 99.1%/98.4%, prompt 100%/100%, startup 90.0%/89.3% — all
comfortably past the 80% DoD bar. The genuine gaps are **repl.rs** (85.8%/81.5%),
**shell.rs** (83.8%/81.4%), and **notification.rs** (82.5%/78.7%).

Two parts of the task title are stale against the actual design:
- **"non-interactive e2e"** — `main.rs` documents (and upstream `mtui/main.py`
  confirms) there is **no** positional/single-command CLI mode; single-command
  dispatch is an `mtui-mcp`/`run_once` concern. Nothing in the `mtui` binary to
  e2e. The closest real analogue is driving the REPL via **piped stdin** and
  asserting the **EOF path exits 0**.
- **unit tests** — already present; only the thin modules need topping up.

The uncovered `repl.rs` lines (143–191) are the `run()` loop body itself — the
`Signal::Success` / shell-intercept / `CtrlC` / `CtrlD` arms — which the `step()`
unit seam deliberately bypasses. They are only reachable by driving the real
`Reedline` editor, i.e. the binary over piped stdin. So the EOF smoke test and
the repl.rs gap are the *same* instrument.

## Plan

1. [small] **Add a piped-stdin / EOF smoke test** to
   `crates/mtui-cli/tests/cli_smoke.rs`. Spawn the binary with `-d`, write a
   short script to stdin (e.g. `help\n` then close the pipe → EOF), and assert
   the process **exits 0** and stderr carries the `mtui starting` breadcrumb (not
   a panic / non-zero). This exercises the real `Reedline` read loop's
   `Signal::Success` + `Signal::CtrlD`(EOF) arms — the currently-uncovered
   `repl.rs` 143–191 — through the binary. *Note:* reedline may need a pty; if
   `read_line` refuses a non-tty stdin, fall back to asserting the loop is
   **entered and exits cleanly** (no hang, bounded by a wait-with-timeout), and
   document the pty limitation inline.
   — verify: `cargo test -p mtui-cli --test cli_smoke` green; the new test named
   e.g. `piped_stdin_reaches_repl_and_exits_clean` passes.

2. [small] **Lift `notification.rs` coverage** (currently 78.7% lines; missing
   27–28, 60–61, 84–86). Add unit tests for the headless/`display`-fallback
   branches and the `desktop_available` false path so both arms of
   `notify_user` are exercised without a real desktop bus.
   — verify: `notification.rs` line coverage ≥ 90%; `cargo test -p mtui-cli`
   green.

3. [small] **Lift `shell.rs` coverage where hermetically reachable** (currently
   81.4%; PTY-bound branches around 122–249, 297–310). Add tests for the
   non-PTY-dependent logic — `is_shell_line` edge cases, `InputEvent`
   classification, and the headless/no-tty early-return path of `run_shell` —
   without spawning a real PTY. Leave genuinely PTY-only lines uncovered with a
   one-line justification comment (real-tty gated), since forcing them would need
   a pseudo-terminal harness out of scope for P6.8.
   — verify: `shell.rs` line coverage improves toward ≥ 85%; no PTY spawned in
   the offline test run; `cargo test -p mtui-cli` green.

4. [trivial] **Update the smoke-test module doc** in `cli_smoke.rs` to state that
   there is no non-interactive single-command mode and that the new test covers
   the piped-stdin/EOF loop exit — so the "non-interactive e2e" line in the task
   is reconciled in-code rather than left dangling.
   — verify: doc comment reads true against `main.rs`; `cargo fmt --check` clean.

5. [trivial] **Record the scope reconciliation on the bead** (`br update
   mtui-rs-lhz.8` note / close reason): unit coverage for
   completion/history/highlighter already landed with P6.3–P6.7; P6.8 added the
   EOF smoke test + gap-fill for the three thin modules; "non-interactive e2e"
   is N/A (no such CLI mode, matching upstream).
   — verify: bead note present; `br epic status` shows Phase 6 at 9/10 (then
   10/10 once the parent closes).

6. [small] **Run the full workspace gate** (DoD hard rule — CI-observed green,
   not predicted):
   `cargo fmt --all --check` **and**
   `cargo clippy --workspace --all-targets -- -D warnings` **and**
   `cargo test --workspace`. Also confirm both surfaces build:
   `cargo build -p mtui-cli` and `cargo build -p mtui-mcp --features mcp`.
   — verify: all four commands exit 0; report status from the actual run.

## Files

- Modify: `crates/mtui-cli/tests/cli_smoke.rs` (EOF smoke test + module doc).
- Modify: `crates/mtui-cli/src/notification.rs` (add `#[cfg(test)]` cases).
- Modify: `crates/mtui-cli/src/shell.rs` (add `#[cfg(test)]` cases).
- Create: none (tests are colocated / in the existing smoke file).
- Delete: none.

## Risks

- **Reedline may reject a non-TTY stdin**, so the EOF test can't drive real input
  lines. Mitigation: the fallback in step 1 (assert clean entry+exit, bounded by
  a wait timeout) still covers the loop-entry/exit arms; document the limitation.
  Do **not** add a pty dependency for P6.8 — that's scope creep past the 80% bar.
- **shell.rs PTY branches are not hermetically reachable.** Forcing them risks a
  flaky/tty-dependent test. Mitigation: cover only the non-PTY logic; justify the
  remaining lines inline rather than chasing 100%.
- **Coverage numbers are a moving target** if clippy/fmt reflow lines. Mitigation:
  run the gate last (step 6) after all edits settle.

## Alternatives considered

- **Add a real non-interactive single-command CLI mode, then e2e it** — rejected:
  out of scope for a test task, contradicts the deliberate design in `main.rs`
  and upstream (`run_once` is the headless surface, exercised in mtui-core/mcp),
  and would break the "surgical changes" rule.
- **Full re-audit pushing every module to 90%+** — rejected by the user in favour
  of gap-fill; every module already clears the 80% DoD, so a blanket push would
  touch already-passing code for marginal gain.
- **EOF smoke only, close as mostly-done** — rejected: leaves notification.rs
  below a comfortable margin and skips the cheap shell.rs/notification.rs wins.

## Success criteria

- [ ] New `cli_smoke.rs` test drives the binary over piped stdin and asserts a
      clean **exit 0** via the EOF path (or the documented fallback), exercising
      `repl.rs::run` loop arms.
- [ ] `notification.rs` line coverage ≥ 90%.
- [ ] `shell.rs` line coverage improved (target ≥ 85%), remaining PTY-only lines
      justified inline.
- [ ] `cli_smoke.rs` module doc reconciles the "non-interactive e2e" wording with
      the actual (no single-command CLI mode) design.
- [ ] `cargo fmt --all --check` clean.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean.
- [ ] `cargo test --workspace` green (was 58 in mtui-cli; now more).
- [ ] Both surfaces build: `cargo build -p mtui-cli` and
      `cargo build -p mtui-mcp --features mcp`.
- [ ] Bead `mtui-rs-lhz.8` carries the scope-reconciliation note and Phase 6
      epic reaches 10/10 (parent closed).
