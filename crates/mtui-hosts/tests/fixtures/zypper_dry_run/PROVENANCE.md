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
| `tw_rm_libcurl4.xml` | `opensuse/tumbleweed` (1.14.102) | `zypper -x -n rm --dry-run libcurl4` | 0 |

The `<install-summary>` layout read here (`<to-remove>` holding one
`<solvable type="package" name="…">` per package) is unchanged in zypper's
`Summary::dumpAsXmlTo` from 1.11.0 (SLE 12) through 1.14.x; no SLE 11 zypper was
checked.

All captures are from non-transactional containers. No dry run was captured on
a transactional host (SL Micro, transactional SLES 16), so the guard leaves
those hosts unchecked.
