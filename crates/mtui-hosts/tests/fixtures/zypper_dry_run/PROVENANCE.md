# zypper dry-run fixtures — provenance

Captured 2026-11-26 from throwaway `podman` containers (aarch64), stdout of the
command byte for byte, for the removal-guard tests in
`crates/mtui-hosts/src/target/removal.rs`.

| file | image (zypper) | command | exit |
|---|---|---|---|
| `leap156_rm_libcurl4.xml` | `opensuse/leap:15.6` (1.14.94) | `zypper -x -n rm --dry-run libcurl4` | 0 |
| `leap156_rm_curl.xml` | `opensuse/leap:15.6` (1.14.94) | `zypper -x -n rm --dry-run curl` | 0 |
| `leap156_rm_libzypp.xml` | `opensuse/leap:15.6` (1.14.94) | `zypper -x -n rm --dry-run libzypp` | 0 |
| `leap156_in_busybox_less.xml` | `opensuse/leap:15.6` (1.14.94) | `zypper -x -n in --dry-run -y -l --force-resolution busybox-less` | 0 |
| `leap156_downgrade_less.xml` | `opensuse/leap:15.6` (1.14.94) | `zypper -x -n in --dry-run -C --force-resolution --oldpackage -y less=643-150600.1.37` | 0 |
| `leap156_in_bash_nothing_to_do.xml` | `opensuse/leap:15.6` (1.14.94) | `zypper -x -n in --dry-run -y -l bash` (refreshes first) | 0 |
| `leap156_rm_missing.xml` | `opensuse/leap:15.6` (1.14.94) | `zypper -x -n --no-refresh rm --dry-run nonexistent-pkg-xyz` | 104 |
| `leap156_rm_glibc_problem.xml` | `opensuse/leap:15.6` (1.14.94) | `zypper -x -n rm --dry-run glibc` | 4 |
| `leap156_in_less_nonexistent.xml` | `opensuse/leap:15.6` (1.14.94), the image's linux/amd64 build run under emulation on an aarch64 host, captured 2026-11-27 | `zypper -x -n in --dry-run -y -l less nonexistent-pkg-xyz` (refreshes first) | 104 |
| `tw_rm_libcurl4.xml` | `opensuse/tumbleweed` (1.14.102) | `zypper -x -n rm --dry-run libcurl4` | 0 |

The `<install-summary>` layout read here (`<to-remove>` holding one
`<solvable type="package" name="…">` per package) is unchanged in zypper's
`Summary::dumpAsXmlTo` from 1.11.0 (SLE 12) through 1.14.x; no SLE 11 zypper was
checked.

All captures are from non-transactional containers. No dry run was captured on
a transactional host (SL Micro, transactional SLES 16), so the guard leaves
those hosts unchecked.

## Not captured: YUM

`YUM` hosts (base product `rhel`) stay unchecked: no `yum`/`dnf` output has
been captured, so there is nothing to pin a parser to. Before a parser can
judge them, capture from a real host of each RHEL major in the refhost pool
(the code itself only knows that `rhel` maps to `YUM`), with `LANG=C` and no
TTY, stdout and stderr apart, and the exit code:

| case | command |
|---|---|
| a removal that cascades into a protected package | `yum -y remove --assumeno <pkg>` |
| a removal that takes only its dependants | `yum -y remove --assumeno <pkg>` |
| an install whose package obsoletes an installed one | `yum -y install --assumeno <pkg>` |
| a downgrade | `yum -y downgrade --assumeno <pkg>` |
| nothing to do | `yum -y install --assumeno <installed pkg>` |
| a missing package, and a dependency problem | `yum -y remove --assumeno <missing>`, and a conflicting install |
| a package name longer than the table's name column | any of the above |

They have to answer:

- whether the exit code tells a declined transaction apart from a failed
  resolution (if both exit `1`, the verdict rests on output markers, which then
  need a capture of every failure shape);
- the section headings that list removals in each version (yum 3 on RHEL 7
  and older, dnf 4 on RHEL 8 and 9), and whether they are stable;
- whether a long name wraps onto a second line when stdout is not a terminal,
  which a line parser would misread;
- whether `-y` and `--assumeno` combine as expected, or `-y` has to go.

## Not captured: transactional hosts

SL Micro and transactional SLES 16 hosts stay unchecked for the same reason:
nothing shows that a dry run on the booted root resolves the transaction
`transactional-update` then runs in a new snapshot. Capture, on one host of
each, the `zypper -x -n in|rm --dry-run …` XML on the booted root next to what
`transactional-update -n pkg install|remove …` actually removed in its snapshot
(for a cascading removal and for a clean one), and whether a dry run inside
the snapshot (`transactional-update -n run zypper -x …`) is available and
leaves no snapshot behind.
