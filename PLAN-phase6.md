# Phase 6 — `mtui-cli` (interactive REPL + `mtui` binary) (detailed)

Goal: build the **interactive shell** and ship the first user-facing binary,
`mtui`. Phase 5 delivered the command engine + dispatch API and the *logic* of
completion; Phase 6 wraps it in a `reedline`-based line editor (tab completion,
persistent history + reverse-search, autosuggest, syntax highlighting, a bottom
toolbar) and wires the top-level `main` that chooses interactive vs
single-command mode.

**Size:** large. **Prereqs:** Phase 0–5 (esp. Phase 5's `Command` trait,
`Session`, `complete()` logic, top-level arg parser). **Blocks:** nothing (MCP is
Phase 7, independent). This is the point a usable `mtui` exists (Phases 0–6).

Source grounding (upstream `main`, the interactive parts of `mtui/cli/` +
`mtui/main.py`):

| Python file             | Size  | Rust target                                       |
| ----------------------- | ----- | ------------------------------------------------- |
| `cli/repl.py`           | 17.9K | `repl.rs` — the REPL loop (`CommandPrompt`)        |
| `cli/_completer.py`     | 6.2K  | `completer.rs` — reedline `Completer` adapter      |
| `cli/_lexer.py`         | 5.5K  | `highlighter.rs` — reedline `Highlighter`          |
| `cli/_history.py`       | 5.2K  | `history.rs` — persistent history backend          |
| `cli/term.py`           | 7.1K  | (mostly Phase 5; pager/termsize confirmed here)    |
| `cli/notification.py`   | 3.1K  | `notification.rs` — optional desktop toast          |
| `cli/prompter.py`       | 2.2K  | `prompter.rs` — serialized stdin prompt             |
| `cli/colors/mode.py`    | 1.7K  | (Phase 5 colors; confirm `--color`/`NO_COLOR`)     |
| `main.py`               | 3.3K  | `main.rs` — the `mtui` binary entry                 |

---

## 6.1 Objectives / definition of done

- [ ] `mtui` binary builds and runs; `mtui --help` and `mtui --version` (with dep
      versions) work.
- [ ] **Single-command / non-interactive mode**: `mtui <RRID> <command …>` runs
      one command and exits (via Phase 5 dispatch).
- [ ] **Interactive REPL**: reedline loop with prompt, dispatches lines to the
      Phase 5 engine, `precmd`/`postcmd` hooks, graceful Ctrl-C / Ctrl-D / `quit`.
- [ ] **Tab completion** over the live command registry + per-command
      `complete()` (targets, templates, flags).
- [ ] **History**: persistent file history, reverse-search (Ctrl-R), autosuggest
      from history (→ to accept).
- [ ] **Highlighting**: first token green (known cmd) / red (unknown); flags cyan.
- [ ] **Bottom toolbar** showing the loaded RRID (+ log level).
- [ ] Optional desktop notification (feature `notify`), no-op when headless.
- [ ] Tests: completion/history/highlighter units; REPL smoke; non-interactive e2e.

## 6.2 prompt_toolkit → reedline mapping

Upstream replaced `cmd.Cmd`/readline with **prompt_toolkit**. Rust equivalent is
**`reedline`** (the nushell line editor), which covers the same feature set:

| prompt_toolkit feature (upstream)          | reedline equivalent                    |
| ------------------------------------------ | -------------------------------------- |
| `PromptSession` read loop                  | `Reedline::read_line(&prompt)`         |
| `Completer` (`_completer.py` adapter)      | `impl reedline::Completer`             |
| `Lexer` first-token/flag coloring          | `impl reedline::Highlighter`           |
| `FileHistory` persistent + reverse-search  | `FileBackedHistory` + Ctrl-R menu      |
| autosuggest-from-history (→ accept)        | reedline hinter (`DefaultHinter`)      |
| `bottom_toolbar` (RRID status)             | `Prompt::render_prompt_*` / transient  |
| key bindings (Ctrl-R, quit, EOF)           | reedline `Keybindings` / `Emacs` edit  |

**Decision:** `reedline` (matches feature set, actively maintained). Fallback:
`rustyline` if a specific behavior (e.g. custom toolbar) proves hard — but
reedline's `Prompt` trait handles the toolbar/RRID line cleanly.

## 6.3 REPL loop (`repl.rs`)

Confirmed upstream flow (repl.py):
- `onecmd(line)`: strip → split first token → look up `do_<name>` → dispatch →
  `postcmd`. Unknown command → `logger.warning("unknown command")`.
- `precmd`/`postcmd` hooks run on **every** dispatch path (interactive + the
  `EOFError → "EOF"` shortcut).
- KeyboardInterrupt (Ctrl-C) → skip dispatch, next read cycle. EOF/`quit` → exit.

### Rust plan
```
crates/mtui-cli/src/
├── main.rs         # mtui binary: parse args -> interactive|single-command
├── repl.rs         # Repl { session, line_editor }; run loop
├── completer.rs    # reedline Completer over Phase-5 registry + complete()
├── highlighter.rs  # reedline Highlighter (green/red/cyan)
├── history.rs      # FileBackedHistory (single shared instance per path)
├── prompt.rs       # reedline Prompt impl: prompt string + bottom toolbar (RRID)
├── prompter.rs     # serialized user prompt (Ctrl-safe)
└── notification.rs # optional desktop toast (feature `notify`)
```
- `Repl::run()`:
  ```
  loop {
    match line_editor.read_line(&prompt) {
      Ok(Signal::Success(line)) => { pre(); dispatch(line).await; post(); }
      Ok(Signal::CtrlC)         => continue,          // skip, re-read
      Ok(Signal::CtrlD)         => break,             // EOF -> quit path
      Err(e)                    => { warn!(...); break; }
    }
  }
  ```
- `dispatch(line)` calls Phase 5's `engine::run_line(&mut session, line).await`;
  the engine owns registry lookup + arg parsing + `Command::run`.
- **async in a sync editor:** `reedline::read_line` is blocking. Run the editor
  read on the main thread; drive async command dispatch on a tokio runtime
  (`Handle::block_on` around `dispatch`, or `read_line` in `spawn_blocking`).
  Decide during 6.7 step 2 — simplest is `runtime.block_on(dispatch)` after each
  synchronous read.

## 6.4 Completion (`completer.rs`)

Confirmed: `_completer.py` bridges prompt_toolkit's `Document` to the legacy
`complete(text, line, begidx, endidx)` command method, **preserving the injected
`state`** (`hosts`, `metadata`, `config`).

### Rust plan
- `impl reedline::Completer`: given the current line + cursor, split into
  `(word, line)`, resolve the command, call Phase 5's
  `Command::complete(&session, line, word)` → `Vec<String>` → map to
  `reedline::Suggestion`.
- First-token completion = registry command names; later tokens = per-command
  candidates (targets via `session.targets.names()`, templates, flags). This is
  exactly the data Phase 5's `complete()` already returns — Phase 6 only adapts.

## 6.5 History + highlighter + toolbar

- **History (`history.rs`)**: `FileBackedHistory` at the XDG data/state path
  (from `mtui-config`). Upstream memoizes a single `FileHistory` per path shared
  by REPL + confirmation prompts + `quit` to avoid concurrent-writer corruption —
  replicate with one shared `Arc` history handle.
- **Highlighter (`highlighter.rs`)**: `impl reedline::Highlighter` — first token
  green if in registry else red; tokens starting `-`/`--` cyan; else default.
  Honor `colors_enabled()` (`--color` + `NO_COLOR`, from Phase 5).
- **Prompt/toolbar (`prompt.rs`)**: `impl reedline::Prompt` — main prompt string
  + a status line rendering the loaded RRID (and log level), mirroring
  `_bottom_toolbar`. When no report is loaded (NullReport), show a neutral state.

## 6.6 `main.rs` — the `mtui` binary

Confirmed main.py flow: parse args → set color mode + create logger → build the
`CommandPrompt` → run interactive or dispatch a single command; map
`CalledProcessError`/parse failures to exit codes.

### Rust plan
- Use Phase 5's top-level `clap` parser (`args.rs`) for global flags (RRID/SUT,
  `--config`, log level, `--color`, version).
- Init `tracing-subscriber` with the chosen level + color mode.
- Build `Session` (config, refhosts factory, NullReport, HostsGroup); wire
  Phase-4 Doer/Check registries (via Phase 5 `wiring.rs`).
- If a command/RRID is given non-interactively → `engine::run_line` once, return
  its exit code. Else → `Repl::run()`.
- **`prompter.rs`**: upstream serializes stdin across SSH worker threads with a
  `threading.Lock`. In the async model, host fan-out are tokio tasks; serialize
  interactive prompts behind a `tokio::sync::Mutex` (or route all prompts to the
  main task via a channel). Confirm during step 5.

## 6.7 Task breakdown

1. `mtui` bin skeleton: clap parse → `--help`/`--version` (w/ dep versions) → exit.
2. `Repl` struct + reedline read loop + dispatch to Phase 5 engine (Ctrl-C/D).
3. `completer.rs`: registry + per-command `complete()` adapter.
4. `history.rs`: shared `FileBackedHistory` + reverse-search + hinter.
5. `prompt.rs` toolbar (RRID) + `highlighter.rs` (green/red/cyan) + color mode.
6. `prompter.rs` serialized prompts; `notification.rs` (feature `notify`).
7. Non-interactive single-command mode wiring + exit codes.
8. Tests: completion/history/highlighter units, REPL smoke, non-interactive e2e.

## 6.8 Test strategy

- **Completion** (`completer.rs`): given a line, assert candidate list matches
  Phase-5 `complete()` output (registry names, targets, flags).
- **History**: write/read round-trip; single-instance sharing (no corruption).
- **Highlighter**: token → style mapping (known/unknown command, flags).
- **REPL smoke**: feed a scripted line sequence via a stubbed line source; assert
  dispatch + `pre/postcmd` order + quit on EOF.
- **Non-interactive e2e**: `mtui --help`, `mtui --version`, and one real command
  against the Phase-2 sshd fixture (`run "uname -a"`), asserting exit code + output.
- Ports/adapts: `test_template_completion.py`, `test_template_registry.py`,
  `test_cmd_quit.py`, plus any prompt_toolkit-specific tests reframed for reedline.

## 6.9 Deliverables (files)

- **Create:** `crates/mtui-cli/src/{main.rs, repl.rs, completer.rs, highlighter.rs,`
  `history.rs, prompt.rs, prompter.rs, notification.rs}` + `crates/mtui-cli/tests/**`.
- **Modify:** `crates/mtui-cli/Cargo.toml` — `[[bin]] name="mtui"`; deps
  `mtui-core` (+ lower crates), `reedline`, `clap`, `tokio`, `tracing`,
  `tracing-subscriber`, color crate; feature `notify` → `notify-rust`; feature
  `pager` → `minus`.
- **Delete:** the original root `src/main.rs` stub if still present from Phase 0.

## 6.10 Risks / decisions to confirm

- **Async ↔ blocking editor.** `reedline::read_line` is synchronous; command
  dispatch is async. Bridge with `runtime.block_on(dispatch)` per line (simplest)
  or `spawn_blocking` the read. Confirm no deadlock with in-flight host tasks.
- **reedline vs rustyline.** reedline chosen for toolbar/hinter/highlighter
  parity with prompt_toolkit; if a needed key binding or the RRID toolbar is
  awkward, rustyline is the fallback (but has weaker hint/highlight support).
- **argparse-in-REPL semantics (from Phase 5).** Per-command `--help` and bad
  args must **not** exit the process inside the REPL. Ensure the Phase-5 clap
  wrapper returns errors instead of `exit()`; Phase 6 renders them.
- **stdin serialization under async fan-out.** Interactive prompts during a
  parallel `run`/`update` must not interleave; serialize via a single prompt
  owner (mutex or channel to the main task).
- **Pager/TTY.** `page()` needs a TTY; detect and fall back to plain stdout when
  piped (also matters for non-interactive mode).
- **Desktop notifications.** `notify-rust` on macOS/Linux differs; keep it a
  no-op when headless/piped (match upstream), gate behind `notify` feature.
- **History file location** — align with XDG state dir; decide compatibility with
  Python mtui's history path if users switch back and forth.

## 6.11 Out of scope for Phase 6

No MCP server (**Phase 7**), no packaging/completions generation/release
(**Phase 8**), no new command logic (all commands are Phase 5). Phase 6 delivers
the interactive `mtui` binary: a reedline REPL + non-interactive dispatch over
the Phase-5 engine.
