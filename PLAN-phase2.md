# Phase 2 — `mtui-hosts` (SSH core) (detailed)

Goal: port the **highest-risk subsystem** — the SSH/SFTP connection layer, the
`Target`/`HostsGroup` model, cooperative remote locking, parallel/serial fan-out
execution, and in-process host arbitration. This is where paramiko's blocking,
thread-pool model must be re-expressed in Rust's async (`tokio` + `russh`) world.

**Size:** large. **Blocks:** Phases 5–7 (commands, REPL, MCP). **Prereqs:**
Phase 0 (workspace), Phase 1 (`mtui-types`, `mtui-config`).

Source grounding (upstream `main`, `mtui/hosts/` — the SSH core; note
`hosts/refhost/` = refhost *resolution* and is **Phase 3**, not here):

| Python file                      | Size  | Rust target                                          |
| -------------------------------- | ----- | ---------------------------------------------------- |
| `connection/connection.py`       | 29.9K | `connection.rs` — russh/sftp wrapper                 |
| `target/hostgroup.py`            | 28.9K | `hostgroup.rs` — group model + parallel/serial       |
| `target/target.py`              | 23.6K | `target.rs` — single host + state machine            |
| `target/locks.py`               | 17.5K | `locks.rs` — remote /var/lock/mtui.lock protocol     |
| `target/actions.py`             | 9.1K  | `actions.rs` — fan-out over targets                  |
| `host_arbiter.py`               | 5.6K  | `arbiter.rs` — in-process host arbitration           |
| `target/repo_manager.py`        | 5.8K  | `repo_manager.rs`                                    |
| `target/parsers/system.py`      | 5.1K  | `parsers/system.rs` — parse `System` from host       |
| `target/operation.py`           | 5.0K  | `operation.rs` — lock→run→check→reboot→unlock        |
| `target/reporter.py`            | 3.6K  | `reporter.rs`                                        |
| `target/package_querier.py`     | 2.7K  | `package_querier.rs`                                 |
| `connection/timeout.py`         | 1.5K  | `connection/timeout.rs` — CommandTimeout + policy    |
| `target/parsers/product.py`     | 1.5K  | `parsers/product.rs`                                 |
| `support/concurrency.py`        | 2.7K  | (obsoleted by tokio — see 2.5)                       |

---

## 2.1 Objectives / definition of done

- [ ] `Connection` connects over SSH (pubkey-only), runs commands with timeout,
      captures stdout/stderr/exit status, and does SFTP put/get.
- [ ] Interactive shell (PTY) works against a real host (or documented as a
      later sub-task if TTY plumbing is deferred).
- [ ] `Target` state machine (enabled/dryrun/disabled) + connect/disconnect.
- [ ] `HostsGroup` runs a command across N targets in **parallel** and **serial**
      modes, honoring per-host state, aggregating results.
- [ ] Remote lock acquire/release + stale-lock reaping matches the on-disk format.
- [ ] `HostArbiter` prevents two in-process owners from claiming the same host.
- [ ] Integration test: `run` across ≥2 hosts against a local `sshd` fixture,
      green. Lock/arbiter unit + integration tests pass.

## 2.2 The core architectural shift: threads → async

Upstream is **blocking paramiko + `ThreadPoolExecutor`**. Rust target is
**async `russh` + `tokio` tasks**. This reshapes several modules:

| Python mechanism                                   | Rust equivalent                                  |
| -------------------------------------------------- | ------------------------------------------------ |
| `paramiko.SSHClient` (blocking)                    | `russh::client` (async) + `russh-sftp`           |
| `ThreadPoolExecutor` + `as_completed` (actions.py) | `futures::stream::FuturesUnordered` / `join_all` |
| `threading.Lock` (shared mutable in actions)       | `tokio::sync::Mutex` / `Arc<Mutex<_>>`           |
| `threading.Condition` wait budget (arbiter)        | `tokio::sync::Notify` + `timeout`                |
| `contextvars` propagation (`ContextExecutor`)      | not needed — pass state explicitly into tasks    |
| `select`/`termios`/`tty` interactive PTY           | `russh` channel + `crossterm` raw mode (see 2.6) |
| blocking command timeout                           | `tokio::time::timeout` around channel read       |

**Design rule:** the whole crate is `async`. Serial-vs-parallel becomes a choice
between awaiting tasks one-by-one vs `FuturesUnordered`; enabled/disabled/dryrun
is a filter+branch before spawning.

## 2.3 Module layout

```
crates/mtui-hosts/src/
├── lib.rs
├── connection/
│   ├── mod.rs          # Connection: connect, run, sftp_put/get, shell
│   ├── timeout.rs      # CommandTimeoutError + host-key policy mapping
│   └── shell.rs        # interactive PTY (feature-gated, see 2.6)
├── target/
│   ├── mod.rs          # Target: state machine, connect/disconnect
│   ├── hostgroup.rs    # HostsGroup: map<hostname,Target> + run/parallel/serial
│   ├── locks.rs        # TargetLock: /var/lock/mtui.lock protocol
│   ├── actions.rs      # fan-out primitives over targets
│   ├── operation.rs    # Operation trait: lock→run→check→reboot→unlock
│   ├── repo_manager.rs
│   ├── reporter.rs
│   ├── package_querier.rs
│   └── parsers/{mod.rs, system.rs, product.rs}
└── arbiter.rs          # HostArbiter: in-process (hostname -> owner-key) map
```

## 2.4 `Connection` (connection.rs) — the crux

Confirmed upstream behavior:
- **Auth is SSH public-key ONLY.** No password fallback — key auth failure
  propagates. (Match this; do not add password auth.)
- **Host-key policy** from config `ssh_strict_host_key_checking` (default
  `auto_add`); `policy_from_config()` maps name → policy. Port as a
  `HostKeyPolicy { AutoAdd, Reject, ... }` enum + a `russh` handler.
- `run(command, timeout)` → capture stdout, stderr, exit status; a per-command
  timeout with an optional interactive **timeout prompt** (ask user to extend).
- `sftp_put(local, remote)` / `sftp_get(remote, local)` via `russh-sftp`.
- Interactive shell using PTY (termios/tty/select in Python).

### Rust plan
- `russh::client::connect` + a `client::Handler` implementing host-key check per
  policy; load keys from agent + `~/.ssh/*` (match paramiko's key discovery).
- `run`: open channel, `exec`, read `data`/`extended_data` (stderr) until EOF,
  wrap read in `tokio::time::timeout`; on timeout, invoke the optional prompt
  callback to extend or abort. Return `{ stdout, stderr, exit_status, run_time }`
  (port `types::commandlog`/`hostlog`).
- SFTP: `russh_sftp::client::SftpSession` for put/get (+ recursive folder mode
  used by the `get` command later).
- **Risk mitigation:** if `russh` friction is high (agent auth, key formats,
  known_hosts), fall back to `ssh2` (libssh2, blocking) wrapped in
  `tokio::task::spawn_blocking`. Keep the `Connection` trait stable so the
  backend is swappable.

## 2.5 `HostsGroup` + `actions` — parallel/serial fan-out

Confirmed: `HostsGroup` is a `UserDict` of `Target`; actions use
`ThreadPoolExecutor` + `as_completed` guarded by a `threading.Lock`;
`ExecutionMode` = parallel | serial; `TargetState` = enabled | dryrun | disabled.

### Rust plan
- `HostsGroup { targets: IndexMap<String, Target> }` (ordered map for stable
  output).
- `run(cmd, mode)`:
  - filter targets by `TargetState` (skip disabled; dryrun = log-only);
  - **parallel:** `FuturesUnordered` / `join_all` of `target.run(cmd)`;
  - **serial:** sequential `for … await` barrier;
  - aggregate `Vec<(hostname, CommandResult)>`; shared collectors use
    `tokio::sync::Mutex` (replaces `threading.Lock`).
- `support/concurrency.py`'s `ContextExecutor` (contextvars propagation) is
  **not ported** — Rust passes state explicitly into spawned futures. Note this
  in docs so reviewers don't look for it.

## 2.6 Interactive PTY shell (highest plumbing risk)

Python uses `termios`/`tty`/`select` to put the local terminal in raw mode and
pump bytes to/from a paramiko channel. Rust plan:
- `russh` request-pty + shell channel; local raw mode via **`crossterm`** (or
  `termios` crate); bidirectional pump with `tokio::select!`.
- **Feature-gate** behind `shell` and treat as a **sub-task that can slip** to
  Phase 6 (REPL) without blocking the batch/`run` path. The non-interactive
  `run` path is the Phase 2 gating deliverable.

## 2.7 Locks (locks.rs)

Confirmed: cooperative remote lock at `/var/lock/mtui.lock`, keyed on
`(user, pid)`, timestamped; `reap_if_stale()` force-removes when older than
`lock_stale_age` (only if `lock_reap_stale` enabled). Autolock of every connected
host while a PI is under test (`pi_autolock`).

### Rust plan
- `TargetLock` writes/reads the lock file **over SFTP/exec on the remote** (it's
  a remote file, not local). Preserve the exact serialized format (user, pid,
  timestamp, RRID) so a Rust mtui and a Python mtui interoperate on the same
  fleet — **format is a compatibility contract; snapshot-test it.**
- `acquire`, `release`, `is_locked`, `reap_if_stale` (gated by config).
- `timestamp()` helper from `support/fileops.py` → port to `mtui-types`/util.

## 2.8 `HostArbiter` (arbiter.rs)

Confirmed: process-global, thread-safe `hostname -> owner-key` map; owner-key =
`(registry_id, RRID)`; a claim that finds all candidates held **waits on a
`Condition`** until release/reap or the `[lock] wait` budget expires.

### Rust plan
- `Arc<Mutex<HashMap<String, OwnerKey>>>` + `tokio::sync::Notify` for the wait;
  `try_claim` / `claim_with_budget(Duration)` / `release`.
- Port `test_target_try_claim.py` + `test_host_arbiter.py` semantics.

## 2.9 `Operation` (operation.rs)

Confirmed: template-method `lock → run → check → reboot → unlock` shared by
install/uninstall. Rust: an `Operation` **trait** with two hook methods
(`doer`, `check`); a default `run(group)` driving the skeleton. Concrete
install/downgrade operations land in Phase 5 but the skeleton + trait belong here.

## 2.10 Test strategy

| Upstream test                    | Ports to                                              |
| -------------------------------- | ---------------------------------------------------- |
| `test_connection.py` (36.7K)     | Connection run/sftp/timeout unit + integ (sshd)      |
| `test_connection_deep.py` (11K)  | edge cases (auth failure, exit codes, stderr)        |
| `test_hostgroup.py` (36.5K)      | parallel/serial, state filtering, aggregation        |
| `test_target.py` (32K)           | Target state machine, connect/disconnect             |
| `test_locks.py` (21.8K)          | lock format, acquire/release, stale reap             |
| `test_host_arbiter.py` +         | arbiter claim/wait/release                            |
| `test_target_try_claim.py`       |                                                      |
| `test_concurrency.py`            | fan-out ordering/aggregation (adapted to tokio)      |
| `test_target_parsers.py` (14.9K) | system/product parsers from host output              |

### Test infrastructure
- **sshd fixture:** a `testcontainers`-driven or docker-compose `sshd` container
  with a known keypair; integration tests marked `#[ignore]` unless `DOCKER`/CI
  env present. Provides real `run`/SFTP/lock-file targets.
- **Mock backend:** a `Connection` trait impl returning canned output for fast
  unit tests of `HostsGroup`/actions/arbiter without a network (mirrors how the
  Python tests stub the connection).
- Lock-file **golden snapshot** (`insta`) to freeze the on-disk format contract.

## 2.11 Task breakdown

1. Define the `Connection` **trait** + result types (`CommandResult`, from
   `types::hostlog`/`commandlog`); add a `MockConnection`.
2. `connection/timeout.rs` (CommandTimeout + `HostKeyPolicy`).
3. `Connection` russh impl: connect (pubkey/agent) → `run` w/ timeout → SFTP.
4. `Target` state machine over `Connection` (enabled/dryrun/disabled, connect).
5. `HostsGroup` + `actions`: parallel (`FuturesUnordered`) + serial fan-out.
6. `locks.rs`: remote lock protocol + stale reap + golden snapshot test.
7. `arbiter.rs`: in-process claim/wait/release.
8. `parsers/` (system, product) from host output; `package_querier`, `reporter`,
   `repo_manager`.
9. `operation.rs` skeleton + trait.
10. Interactive `shell.rs` (feature `shell`) — may slip to Phase 6.
11. sshd integration fixture + full test port.

## 2.12 Deliverables (files)

- **Create:** the `crates/mtui-hosts/src/**` tree above +
  `crates/mtui-hosts/tests/**` (mock-based unit + sshd integration) + fixtures
  (keypair, expected lock file).
- **Modify:** `crates/mtui-hosts/Cargo.toml` — add `russh`, `russh-sftp`,
  `tokio`, `futures`, `indexmap`, `crossterm` (feature `shell`), `tracing`;
  dev-deps `insta`, `testcontainers`; optional `ssh2` (feature `ssh2-backend`).

## 2.13 Risks / decisions to confirm

- **`russh` vs `ssh2`.** `russh` is the async-native default but lower-level
  (agent auth, key formats, known_hosts hostname hashing all manual). Keep the
  `Connection` trait so `ssh2` (blocking, via `spawn_blocking`) is a drop-in
  fallback. **Decision gate:** spike `russh` connect+run+sftp against the sshd
  fixture in step 3 before committing.
- **Lock-file format compatibility.** If Rust and Python mtui must share a fleet,
  the `/var/lock/mtui.lock` serialization is a hard contract — snapshot it and do
  not "improve" it.
- **Interactive PTY** is the riskiest plumbing; explicitly de-risked by
  feature-gating and allowing slip to Phase 6.
- **Arbiter scope.** In one Rust process (REPL or MCP), the arbiter is in-memory.
  Cross-process safety still relies on the remote lock file (same as Python).
- **`contextvars` behavior not ported** — confirm nothing downstream depends on
  implicit context propagation (logging correlation IDs, etc.); if it does,
  thread an explicit `tracing::Span` through tasks instead.

## 2.14 Out of scope for Phase 2

No refhost **resolution** (fetch/cache of refhosts.yml → Phase 3), no command
definitions (`run`/`update`/… → Phase 5), no REPL, no MCP. Phase 2 delivers the
connection/target/group/lock/arbiter machinery and proves it against a live sshd.
