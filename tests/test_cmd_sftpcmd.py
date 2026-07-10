"""Tests for the `put` (SFTPPut) and `get` (SFTPGet) commands."""

from __future__ import annotations

import logging
from argparse import Namespace
from pathlib import Path
from unittest.mock import MagicMock

from mtui.commands.sftpcmd import SFTPGet, SFTPPut
from mtui.hosts.target.hostgroup import HostsGroup


def _target(hostname):
    t = MagicMock()
    t.hostname = hostname
    t.state = "enabled"
    return t


def _prompt(hg) -> MagicMock:
    p = MagicMock()
    p.metadata = MagicMock()
    p.metadata.target_wd.return_value = Path("/remote/x")
    p.display = MagicMock()
    p.targets = hg
    return p


def test_sftp_put_uploads_real_file(mock_config, tmp_path):
    fp = tmp_path / "realfile"
    fp.write_text("hi")
    t = _target("h1")
    hg = HostsGroup([t])
    prompt = _prompt(hg)
    args = Namespace(filename=[str(fp)])

    # Patch the sftp_put method on the actual HostsGroup so the assertion
    # operates on a known instance (the enabled-selection returns a fresh group
    # with the same target inside; we capture it through the target instead).
    sent: list[tuple[Path, Path]] = []

    def fake_sftp(self, local, remote):
        sent.append((local, remote))

    from mtui.hosts.target.hostgroup import HostsGroup as HG

    original = HG.sftp_put
    HG.sftp_put = fake_sftp  # ty: ignore[invalid-assignment]
    try:
        SFTPPut(args, mock_config, MagicMock(), prompt)()
    finally:
        HG.sftp_put = original

    assert sent
    assert sent[0][0] == fp


def test_sftp_put_missing_file_logs_error(mock_config, caplog):
    prompt = _prompt(HostsGroup([]))
    args = Namespace(filename=["/nonexistent/path/that/does/not/exist"])
    caplog.set_level(logging.ERROR, logger="mtui.command.sftp")

    SFTPPut(args, mock_config, MagicMock(), prompt)()

    assert any("not found" in r.message for r in caplog.records)


def test_sftp_get_only_targets_enabled_hosts(mock_config):
    """`get` must skip disabled hosts (parity with `put` and the docstring)."""
    enabled = _target("h1")
    disabled = _target("h2")
    disabled.state = "disabled"
    prompt = _prompt(HostsGroup([enabled, disabled]))
    args = Namespace(filename=[Path("/remote/file")])

    SFTPGet(args, mock_config, MagicMock(), prompt)()

    # perform_get receives a group containing only the enabled host.
    targets = prompt.metadata.perform_get.call_args.args[0]
    assert targets.names() == ["h1"]


def test_sftp_put_directory_preserves_tree(mock_config, tmp_path):
    """`put mydir/` keeps the tree; same-named files must not clobber.

    The walk used to upload every nested file to target_wd(basename), so
    d/sub1/test.sh and d/sub2/test.sh both landed on the same remote path
    and the second silently overwrote the first.
    """
    d = tmp_path / "mydir"
    (d / "sub1").mkdir(parents=True)
    (d / "sub2").mkdir()
    (d / "sub1" / "test.sh").write_text("one")
    (d / "sub2" / "test.sh").write_text("two")

    t = _target("h1")
    hg = HostsGroup([t])
    prompt = _prompt(hg)
    prompt.metadata.target_wd.side_effect = lambda *p: Path("/remote").joinpath(*p)
    args = Namespace(filename=[str(d)])

    sent: list[tuple[Path, Path]] = []

    def fake_sftp(self, local, remote):
        sent.append((local, remote))

    from mtui.hosts.target.hostgroup import HostsGroup as HG

    original = HG.sftp_put
    HG.sftp_put = fake_sftp  # ty: ignore[invalid-assignment]
    try:
        SFTPPut(args, mock_config, MagicMock(), prompt)()
    finally:
        HG.sftp_put = original

    remotes = sorted(str(r) for _, r in sent)
    assert remotes == [
        "/remote/mydir/sub1/test.sh",
        "/remote/mydir/sub2/test.sh",
    ]
    assert len(set(remotes)) == 2  # no clobbering


def test_sftp_put_dotdot_stays_inside_working_directory(
    mock_config, tmp_path, monkeypatch
):
    """`put ..` must not escape the run's remote working directory.

    A literal '..' argument survived relative_to() as a '..' path part,
    so the remote path resolved OUTSIDE the id-scoped directory into the
    shared target_tempdir (adversarial-review catch on the tree fix).
    """
    base = tmp_path / "parent"
    (base / "cwd").mkdir(parents=True)
    (base / "top.txt").write_text("x")
    monkeypatch.chdir(base / "cwd")

    t = _target("h1")
    hg = HostsGroup([t])
    prompt = _prompt(hg)
    prompt.metadata.target_wd.side_effect = lambda *p: Path("/remote").joinpath(*p)
    args = Namespace(filename=[".."])

    sent: list[tuple[Path, Path]] = []

    def fake_sftp(self, local, remote):
        sent.append((local, remote))

    from mtui.hosts.target.hostgroup import HostsGroup as HG

    original = HG.sftp_put
    HG.sftp_put = fake_sftp  # ty: ignore[invalid-assignment]
    try:
        SFTPPut(args, mock_config, MagicMock(), prompt)()
    finally:
        HG.sftp_put = original

    assert sent
    for _, remote in sent:
        assert ".." not in remote.parts  # confined under /remote
        assert str(remote).startswith("/remote/parent/")
