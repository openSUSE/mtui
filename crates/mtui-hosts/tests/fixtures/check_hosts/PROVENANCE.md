# `check_hosts` probes — provenance

No host output has been captured for `check_hosts`. Every test input in
`crates/mtui-hosts/src/target/health.rs` and
`crates/mtui-core/src/commands/check_hosts.rs` is **hand-written**; the table
below says what each reading rests on and what is unverified.

| check | command sent | status | what the reading rests on |
|---|---|---|---|
| `packages` | `LC_ALL=C rpm -q --queryformat "%{Name} %{Version}-%{Release}\n" -- <names>` | supported | the query (shared constant `RPM_QUERY`) and the `package X is not installed` line `PackageQuerier` already parses (`package_querier.rs`); every queried name must be answered |
| `root_space` | `LC_ALL=C df -Pk /` | supported by a standard | the POSIX `df -P` output format (XCU `df`, STDOUT): a header naming `1024-blocks` and one row ending in `/` |
| `failed_units` | `LC_ALL=C systemctl --failed --no-legend --plain --no-pager` | **unverified** | `ok` = exit `0`, empty stdout and empty stderr — not checked against a real host. A listed unit is read only when its first word is a systemd unit name |
| `reboot` | `LC_ALL=C ZYPP_LOCK_TIMEOUT=0 zypper -n needs-rebooting` | **unverified** | the exit code alone: `102` is `ZYPPER_EXIT_INF_REBOOT_NEEDED` (`zypper(8)`, also named in `update_workflow/checks/mod.rs`); `ok` = exit `0` — not checked against a real host |
| `test_repositories` | `LC_ALL=C ZYPP_LOCK_TIMEOUT=0 zypper -n lr` | supported | the `\|`-separated table with the alias in the second column, and exit `6` for "no repositories defined", as the update script's cleanup loop reads them (`update_workflow/actions/update.rs`) |

Any other exit code, a probe that could not run or timed out, output marked
truncated, and output that is not one of these shapes is `unknown`.

## What "read-only" means

The check takes neither the operation lock nor a pool claim, writes no
history row, records nothing in mtui's host log (so neither `show_log` nor
`export` sees the probes), and changes no package or repository. `zypper lr`
and `zypper needs-rebooting` run as root and hold zypper's own lock while
they run, so a concurrent transaction may see it busy for that moment, and
zypper writes its own log. `ZYPP_LOCK_TIMEOUT=0` (from `zypper(8)`, not
verified on a host) is meant to make a probe fail at once, rather than wait,
while someone else holds the lock; the 60 s probe bound applies either way.

## Hand-written fixtures

| fixture | written from | unverified |
|---|---|---|
| `rpm -q` lines `<name> 1.0-1` and `package <name> is not installed` | the query format above and `PackageQuerier`'s parser | the version strings are placeholders |
| `error: cannot open Packages database in /var/lib/rpm` (exit 1) | memory of rpm's wording | the wording and the exit code; the test only needs "no answer for the queried names" |
| the `df -P` header and rows | the POSIX format | the column spacing on SUSE's coreutils |
| `kdump.service loaded failed failed …`, `● snapper-boot.timer …` | `systemctl(1)`'s UNIT LOAD ACTIVE SUB DESCRIPTION columns, from memory | the whole layout under `--no-legend --plain`, including whether a bullet appears |
| `0 loaded units listed.` | memory of systemd's legend | that `--no-legend` suppresses it |
| `Failed to list units: Access denied` on stderr with exit 0 | invented to pin "stderr makes it unknown" | that systemctl ever does this |
| `zypper -n lr` header `# \| Alias \| Name \| Enabled \| GPG Check \| Refresh` and rows | the column order the cleanup loop's `awk -F "\|" '{print $2}'` relies on | the header text and the other columns |
| `System management is locked by the application with pid 4242 (zypper).` | memory of zypper's lock message | the exact text; the tests treat every exit other than `0` and `6` alike |
| `needs-rebooting`, `systemctl`, `zypper lr` with an exit other than the ones read | — | no single exit code is presented as fact: the tests check "any other exit is unknown" over many values |

## Checks that do not apply

These are not sent and are listed per host as not checked; they do not make
the host `unknown`:

- `failed_units` on SLE 11, which has no systemd;
- `reboot` on SLE 11, whose zypper is not known to offer `needs-rebooting`;
- `reboot` on a read-only or undecided root, where nothing shows that
  zypper's reboot flag covers a snapshot `transactional-update` made and the
  host has not booted.

## Captures needed

To verify the two unverified `ok` readings, on SLES 12 SP5 (systemd 228),
SLES 15 SP6 and SLES 16, with `LC_ALL=C`, no TTY, stdout and stderr apart,
and the exit code:

- `systemctl --failed --no-legend --plain --no-pager` with no failed unit and
  with two failed units;
- `zypper -n needs-rebooting` on a freshly booted host, and after an update
  of a package listed in `/etc/zypp/needreboot` before the reboot; also on
  SLES 12 SP5 and SLES 15 SP1 to learn whether the command exists there.

For `reboot` on a read-only root (SL Micro 6.x, SLES 16 read-only), after a
`transactional-update -n pkg install <pkg>` and before the reboot:
`zypper -n needs-rebooting` (exit code), `snapper --no-dbus list`,
`btrfs subvolume get-default /`, and `/proc/cmdline`.

For the issue's kernel comparison (running kernel against the newest
installed one of its flavour): `uname -r` next to `rpm -q --queryformat
"%{Name} %{Version}-%{Release}\n" kernel-<flavour>` on each family, to pin how
the `uname -r` string maps to the package's version and release. The repo
holds no such mapping, which is why `reboot` reads `needs-rebooting` instead.

## Not probed: `dependencies` and `patches`

`zypper verify` and `zypper list-patches` are left out until captures show
how to read them. Capture on one host of each of SLES 12 SP5, SLES 15 SP6,
SLES 16 with a read-write root, SLES 16 with a read-only root and SL Micro
6.x:

| case | command |
|---|---|
| dependencies satisfied | `zypper -n --no-refresh verify --dry-run` and `zypper -x -n --no-refresh verify --dry-run` |
| a broken dependency (e.g. a library removed with `rpm -e --nodeps`) | the same two |
| the zypp lock held by another zypper | the same two, and `zypper -n --no-refresh lp` |
| no needed patch | `zypper -n --no-refresh lp` and `zypper -x -n --no-refresh lp` |
| needed patches, one of them security | the same two |
| one repository unreachable (`106` expected) | `zypper -n lp` |

They have to answer whether `verify --dry-run` exits differently for
satisfied and broken dependencies and which text or XML element says which;
whether `lp` exits `0` when it lists needed patches (`zypper(8)` documents
`100`/`101` only for `patch-check`) and where the count is in the XML; and
whether either command answers on a read-only root without a snapshot.

## Not read: snapper snapshot count

`root_space` does not report the number of snapper snapshots on a btrfs root:
no `snapper list` output has been captured. Capture `snapper --no-dbus
--machine-readable csv list` and `snapper --no-dbus list` on a SLES 15 and a
SLES 16 btrfs host.
