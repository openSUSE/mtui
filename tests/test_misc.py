import logging

from mtui.export.base import BaseExport


class FakeExport(BaseExport):
    def get_logs(self, *args, **kwds):
        return []

    def run(self, *args, **kwds):
        return []


def test_duplicate_found(caplog, log_install) -> None:
    caplog.set_level(logging.INFO, logger="mtui.export.base")

    fn = log_install
    data = fn.read_text().split("\n")

    export = FakeExport("", "", [], False, "123", True)

    export._writer(fn, data)

    assert caplog.records[0].msg == f"Log {fn} exists and is same as export"


def test_diffeerent_files(caplog, log_install) -> None:
    caplog.set_level(logging.INFO, logger="mtui.export.base")

    fn = log_install
    data = ["Non duplicate text of install log", "fake log"]

    export = FakeExport("", "", [], False, "123", False)

    export._writer(fn, data)

    assert caplog.records[0].msg == f"file {fn} exists."
    assert caplog.records[1].msg.startswith("exporting log to")
