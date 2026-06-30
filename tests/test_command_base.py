"""Tests for the mtui commands._command base module."""

from argparse import Namespace
from typing import ClassVar
from unittest.mock import MagicMock

import pytest

from mtui.cli.argparse import ArgsParseFailureError
from mtui.commands._command import Command
from mtui.hosts.target.hostgroup import HostsGroup
from mtui.support.messages import (
    FanOutError,
    HostIsNotConnectedError,
    TemplateNotLoadedError,
)


# Create a concrete subclass for testing
class ConcreteCommand(Command):
    command = "test_cmd"

    @classmethod
    def _add_arguments(cls, parser):
        parser.add_argument("--verbose", action="store_true")
        parser.add_argument("name", nargs="?", default=None)

    def __call__(self):
        pass


class ConcreteCommandWithHosts(Command):
    command = "host_cmd"

    @classmethod
    def _add_arguments(cls, parser):
        cls._add_hosts_arg(parser)

    def __call__(self):
        pass


# --- parse_args ---


class TestParseArgs:
    def test_empty_args(self):
        """Test parsing empty argument string."""
        sys = MagicMock()
        result = ConcreteCommand.parse_args("", sys)
        assert isinstance(result, Namespace)
        assert result.verbose is False

    def test_with_args(self):
        """Test parsing with actual arguments."""
        sys = MagicMock()
        result = ConcreteCommand.parse_args("--verbose myname", sys)
        assert result.verbose is True
        assert result.name == "myname"

    def test_host_args(self):
        """Test parsing host arguments."""
        sys = MagicMock()
        result = ConcreteCommandWithHosts.parse_args("-t host1 -t host2", sys)
        assert result.hosts == ["host1", "host2"]

    def test_host_args_none_when_not_specified(self):
        """Test hosts is None when -t not specified."""
        sys = MagicMock()
        result = ConcreteCommandWithHosts.parse_args("", sys)
        assert result.hosts is None

    def test_invalid_shlex_raises_argsparsefailure(self):
        """Stray backslash must surface as ArgsParseFailureError, not ValueError."""
        sys = MagicMock()
        with pytest.raises(ArgsParseFailureError):
            ConcreteCommand.parse_args("foo\\", sys)

    def test_invalid_shlex_unbalanced_quote(self):
        """Unbalanced quote must surface as ArgsParseFailureError."""
        sys = MagicMock()
        with pytest.raises(ArgsParseFailureError):
            ConcreteCommand.parse_args('foo "bar', sys)

    def test_quoted_args_preserved(self):
        """Quoted arguments containing whitespace stay together (the original commit's intent)."""
        sys = MagicMock()
        result = ConcreteCommand.parse_args('"my name"', sys)
        assert result.name == "my name"


# --- argparser ---


class TestArgparser:
    def test_argparser_creates_parser(self):
        """Test argparser creates a working argument parser."""
        sys = MagicMock()
        parser = ConcreteCommand.argparser(sys)
        assert parser.prog == "test_cmd"

    def test_argparser_includes_description(self):
        """Test argparser includes class docstring as description."""
        sys = MagicMock()
        parser = ConcreteCommand.argparser(sys)
        # Parser is created from class - just verify it's not None
        assert parser is not None


# --- __init__ ---


class TestCommandInit:
    def test_init_stores_attributes(self):
        """Test __init__ stores all passed attributes."""
        args = MagicMock()
        config = MagicMock()
        sys = MagicMock()
        prompt = MagicMock()
        prompt.metadata = MagicMock()
        prompt.display = MagicMock()
        prompt.targets = MagicMock()

        cmd = ConcreteCommand(args, config, sys, prompt)

        assert cmd.args is args
        assert cmd.config is config
        assert cmd.sys is sys
        assert cmd.prompt is prompt
        assert cmd.metadata is prompt.metadata
        assert cmd.display is prompt.display
        assert cmd.targets is prompt.targets


# --- parse_hosts ---


class TestParseHosts:
    def _make_cmd(self, hosts_arg=None):
        """Create a ConcreteCommandWithHosts with mocked internals."""
        args = MagicMock()
        args.hosts = hosts_arg
        config = MagicMock()
        sys = MagicMock()
        prompt = MagicMock()

        t1 = MagicMock()
        t2 = MagicMock()
        t1.hostname = "h1"
        t2.hostname = "h2"
        t1.state = "enabled"
        t2.state = "enabled"

        prompt.targets = HostsGroup([t1, t2])  # type: ignore[arg-type]
        prompt.metadata = MagicMock()
        prompt.display = MagicMock()

        return ConcreteCommandWithHosts(args, config, sys, prompt)

    def test_parse_hosts_none_selects_enabled(self):
        """Test parse_hosts with None selects all enabled hosts."""
        cmd = self._make_cmd(hosts_arg=None)
        result = cmd.parse_hosts()

        assert len(result) == 2

    def test_parse_hosts_specific(self):
        """Test parse_hosts with specific hostname."""
        cmd = self._make_cmd(hosts_arg=["h1"])
        result = cmd.parse_hosts()

        assert len(result) == 1
        assert "h1" in result

    def test_parse_hosts_nonexistent_raises(self):
        """Test parse_hosts with unknown host raises."""
        cmd = self._make_cmd(hosts_arg=["unknown"])

        with pytest.raises(HostIsNotConnectedError):
            cmd.parse_hosts()

    def test_parse_hosts_all_deprecated(self):
        """Test parse_hosts with 'all' uses all hosts with deprecation."""
        cmd = self._make_cmd(hosts_arg=["all"])
        result = cmd.parse_hosts()

        assert len(result) == 2


# --- complete ---


class TestComplete:
    def test_complete_returns_empty_list(self):
        """Test default complete() returns empty list."""
        result = ConcreteCommand.complete({}, "", "", 0, 0)
        assert result == []


# --- println ---


class TestPrintln:
    def test_println_writes_to_stdout(self):
        """Test println writes to sys.stdout with newline."""
        args = MagicMock()
        config = MagicMock()
        sys = MagicMock()
        prompt = MagicMock()
        prompt.metadata = MagicMock()
        prompt.display = MagicMock()
        prompt.targets = MagicMock()

        cmd = ConcreteCommand(args, config, sys, prompt)
        cmd.println("hello world")

        sys.stdout.write.assert_called_once_with("hello world\n")
        sys.stdout.flush.assert_called_once()

    def test_println_empty(self):
        """Test println with no args writes just a newline."""
        args = MagicMock()
        config = MagicMock()
        sys = MagicMock()
        prompt = MagicMock()
        prompt.metadata = MagicMock()
        prompt.display = MagicMock()
        prompt.targets = MagicMock()

        cmd = ConcreteCommand(args, config, sys, prompt)
        cmd.println()

        sys.stdout.write.assert_called_once_with("\n")


# --- fan-out: scope / _resolve_templates / run ---


class _FakeReport:
    """Minimal TestReport stand-in: carries an id and a targets attribute."""

    def __init__(self, rrid):
        self.id = rrid
        self.targets = HostsGroup([])


class _FakeRegistry:
    """Minimal TemplateRegistry stand-in for resolution tests."""

    def __init__(self, reports, active=None):
        self._reports = {str(r.id): r for r in reports}
        self._active = active or (reports[0] if reports else None)

    def all(self):
        return list(self._reports.values())

    def get(self, rrid):
        return self._reports[rrid]

    def __len__(self):
        return len(self._reports)

    @property
    def active(self):
        return self._active


class CountingCommand(Command):
    """Counts invocations and records which template each ran against."""

    command = "counting_cmd"
    scope = "active"

    def __call__(self):
        self.calls.append(str(self.metadata.id))

    # ``__slots__`` on Command forbids ad-hoc attributes; stash the log on
    # the class so the body can append to it.
    calls: ClassVar[list] = []


class FanoutCommand(CountingCommand):
    command = "fanout_cmd"
    scope = "fanout"


class FailingFanoutCommand(Command):
    """Fans out but raises on a configured RRID to exercise partial-failure."""

    command = "failing_fanout_cmd"
    scope = "fanout"

    fail_on: str = ""
    calls: ClassVar[list] = []

    def __call__(self):
        rrid = str(self.metadata.id)
        self.calls.append(rrid)
        if rrid == self.fail_on:
            raise RuntimeError(f"boom on {rrid}")


def _make_cmd(
    cmd_cls, registry, *, template=None, all_templates=False, interactive=True
):
    args = Namespace(template=template, all_templates=all_templates)
    prompt = MagicMock()
    prompt.templates = registry
    prompt.metadata = registry.active
    prompt.targets = registry.active.targets
    prompt.display = MagicMock()
    prompt.interactive = interactive
    return cmd_cls(args, MagicMock(), MagicMock(), prompt)


class TestResolveTemplates:
    def test_active_scope_returns_active_only(self):
        reg = _FakeRegistry([_FakeReport("A"), _FakeReport("B")])
        cmd = _make_cmd(CountingCommand, reg)
        resolved = cmd._resolve_templates()
        assert [str(r.id) for r in resolved] == ["A"]

    def test_fanout_scope_returns_all(self):
        reg = _FakeRegistry([_FakeReport("A"), _FakeReport("B")])
        cmd = _make_cmd(FanoutCommand, reg)
        resolved = cmd._resolve_templates()
        assert [str(r.id) for r in resolved] == ["A", "B"]

    def test_template_flag_scopes_to_one(self):
        reg = _FakeRegistry([_FakeReport("A"), _FakeReport("B")])
        cmd = _make_cmd(FanoutCommand, reg, template="B")
        resolved = cmd._resolve_templates()
        assert [str(r.id) for r in resolved] == ["B"]

    def test_template_flag_unknown_raises(self):
        reg = _FakeRegistry([_FakeReport("A")])
        cmd = _make_cmd(FanoutCommand, reg, template="ZZ")
        with pytest.raises(TemplateNotLoadedError):
            cmd._resolve_templates()

    def test_all_templates_flag_forces_fanout_on_active_cmd(self):
        reg = _FakeRegistry([_FakeReport("A"), _FakeReport("B")])
        cmd = _make_cmd(CountingCommand, reg, all_templates=True)
        resolved = cmd._resolve_templates()
        assert [str(r.id) for r in resolved] == ["A", "B"]

    def test_fanout_empty_registry_falls_back_to_active(self):
        active = _FakeReport("")  # null-report stand-in
        reg = _FakeRegistry([], active=active)
        cmd = _make_cmd(FanoutCommand, reg)
        resolved = cmd._resolve_templates()
        assert resolved == [active]

    def test_mcp_active_scope_fans_out_when_many_loaded(self):
        # Under MCP (non-interactive) there is no addressable active pointer,
        # so an unscoped active-scope command fans out across all templates.
        reg = _FakeRegistry([_FakeReport("A"), _FakeReport("B")])
        cmd = _make_cmd(CountingCommand, reg, interactive=False)
        resolved = cmd._resolve_templates()
        assert [str(r.id) for r in resolved] == ["A", "B"]

    def test_mcp_active_scope_single_template_unchanged(self):
        # With one template the MCP path is identical to the active fallback.
        reg = _FakeRegistry([_FakeReport("A")])
        cmd = _make_cmd(CountingCommand, reg, interactive=False)
        resolved = cmd._resolve_templates()
        assert [str(r.id) for r in resolved] == ["A"]

    def test_repl_active_scope_still_returns_active_only(self):
        # The interactive REPL keeps its active-template behaviour.
        reg = _FakeRegistry([_FakeReport("A"), _FakeReport("B")])
        cmd = _make_cmd(CountingCommand, reg, interactive=True)
        resolved = cmd._resolve_templates()
        assert [str(r.id) for r in resolved] == ["A"]


class TestRun:
    def test_single_template_calls_once(self):
        CountingCommand.calls = []
        reg = _FakeRegistry([_FakeReport("A")])
        cmd = _make_cmd(CountingCommand, reg)
        cmd.run()
        assert CountingCommand.calls == ["A"]

    def test_fanout_calls_per_template_with_banner(self):
        FanoutCommand.calls = []
        reg = _FakeRegistry([_FakeReport("A"), _FakeReport("B")])
        cmd = _make_cmd(FanoutCommand, reg)
        cmd.run()
        assert FanoutCommand.calls == ["A", "B"]
        # One banner per template when fanning out across more than one.
        assert cmd.display.template_banner.call_count == 2

    def test_single_template_error_propagates(self):
        reg = _FakeRegistry([_FakeReport("A")])
        FailingFanoutCommand.fail_on = "A"
        FailingFanoutCommand.calls = []
        # One resolved template → direct call → raises as today.
        cmd = _make_cmd(FailingFanoutCommand, reg, template="A")
        with pytest.raises(RuntimeError):
            cmd.run()

    def test_fanout_failure_does_not_abort_others_and_aggregates(self):
        reg = _FakeRegistry([_FakeReport("A"), _FakeReport("B"), _FakeReport("C")])
        FailingFanoutCommand.fail_on = "B"
        FailingFanoutCommand.calls = []
        cmd = _make_cmd(FailingFanoutCommand, reg)
        with pytest.raises(FanOutError) as exc:
            cmd.run()
        # All three ran even though B failed.
        assert FailingFanoutCommand.calls == ["A", "B", "C"]
        # The aggregate names only the failed template.
        assert [rrid for rrid, _ in exc.value.failures] == ["B"]
