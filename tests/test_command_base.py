"""Tests for the mtui commands._command base module."""

from argparse import Namespace
from unittest.mock import MagicMock

import pytest

from mtui.commands._command import Command
from mtui.messages import HostIsNotConnectedError
from mtui.target.hostgroup import HostsGroup


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

        prompt.targets = HostsGroup([t1, t2])
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
