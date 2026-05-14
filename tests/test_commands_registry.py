"""Characterization tests for the ``mtui.commands`` plugin registry.

These tests pin the exact set of commands the package exposes so that the
Phase 5b / C5 refactor (filesystem-glob + ``globals()`` -> ``__init_subclass__``
registry) cannot silently change the public surface.

The ``EXPECTED`` constant lists every ``(command_string, module, class_name)``
triple the loader produces. The set must stay byte-identical across the
refactor; if a real command is added or renamed in a separate change, update
``EXPECTED`` in the same commit that introduces the change.
"""

from __future__ import annotations

from mtui import commands

# Snapshot taken on phase_five_b2 before the C5 refactor.
EXPECTED: frozenset[tuple[str, str, str]] = frozenset(
    {
        ("EOF", "mtui.commands.quit", "DEOF"),
        ("add_host", "mtui.commands.addhost", "AddHost"),
        ("analyze_diff", "mtui.commands.showdiff", "AnalyzeDiff"),
        ("approve", "mtui.commands.apicall", "Approve"),
        ("assign", "mtui.commands.apicall", "Assign"),
        ("checkout", "mtui.commands.checkout", "Checkout"),
        ("comment", "mtui.commands.apicall", "Comment"),
        ("commit", "mtui.commands.commit", "Commit"),
        ("config", "mtui.commands.config", "Config"),
        ("downgrade", "mtui.commands.downgrade", "Downgrade"),
        ("edit", "mtui.commands.edit", "Edit"),
        ("exit", "mtui.commands.quit", "QExit"),
        ("export", "mtui.commands.export", "Export"),
        ("get", "mtui.commands.sftpcmd", "SFTPGet"),
        ("install", "mtui.commands.zypper", "Install"),
        ("list_bugs", "mtui.commands.simplelists", "ListBugs"),
        ("list_history", "mtui.commands.simplelists", "ListHistory"),
        ("list_hosts", "mtui.commands.simplelists", "ListHosts"),
        ("list_locks", "mtui.commands.simplelists", "ListLocks"),
        ("list_metadata", "mtui.commands.simplelists", "ListMetadata"),
        ("list_packages", "mtui.commands.listpackages", "ListPackages"),
        ("list_products", "mtui.commands.products", "ListProducts"),
        ("list_sessions", "mtui.commands.simplelists", "ListSessions"),
        ("list_timeout", "mtui.commands.simplelists", "ListTimeout"),
        ("list_update_commands", "mtui.commands.simplelists", "ListUpdateCommands"),
        ("list_versions", "mtui.commands.simplelists", "ListVersions"),
        ("load_template", "mtui.commands.loadtemplate", "LoadTemplate"),
        ("lock", "mtui.commands.hostslock", "HostLock"),
        ("lrun", "mtui.commands.localrun", "LocalRun"),
        ("prepare", "mtui.commands.prepare", "Prepare"),
        ("put", "mtui.commands.sftpcmd", "SFTPPut"),
        ("quit", "mtui.commands.quit", "Quit"),
        ("reject", "mtui.commands.apicall", "Reject"),
        ("reload_openqa", "mtui.commands.reloadoqa", "ReloadOpenQA"),
        ("reload_products", "mtui.commands.reload", "ReloadProducts"),
        ("remove_host", "mtui.commands.removehost", "RemoveHost"),
        ("report-bug", "mtui.commands.reportbug", "ReportBug"),
        ("run", "mtui.commands.run", "Run"),
        ("set_host_state", "mtui.commands.hoststate", "HostState"),
        ("set_location", "mtui.commands.simpleset", "SetLocation"),
        ("set_log_level", "mtui.commands.simpleset", "SetLogLevel"),
        ("set_repo", "mtui.commands.setrepo", "SetRepo"),
        ("set_session_name", "mtui.commands.simpleset", "SessionName"),
        ("set_timeout", "mtui.commands.simpleset", "SetTimeout"),
        ("set_workflow", "mtui.commands.simpleset", "SetWorkflow"),
        ("shell", "mtui.commands.shell", "Shell"),
        ("show_diff", "mtui.commands.showdiff", "ShowDiff"),
        ("show_log", "mtui.commands.simplelists", "ListLog"),
        ("show_update_repos", "mtui.commands.showrepos", "Showrepos"),
        ("terms", "mtui.commands.terms", "Terms"),
        ("unassign", "mtui.commands.apicall", "Unassign"),
        ("uninstall", "mtui.commands.zypper", "Uninstall"),
        ("unlock", "mtui.commands.hostsunlock", "HostsUnlock"),
        ("update", "mtui.commands.update", "Update"),
        ("whoami", "mtui.commands.whoami", "Whoami"),
    }
)


def _current_surface() -> frozenset[tuple[str, str, str]]:
    """Return the live ``(command, module, class_name)`` set the loader exposes.

    Reads the legacy ``cmd_list`` + ``getattr`` surface so this test passes
    against ``main`` before the C5 refactor; it is updated in a follow-up
    commit to read ``commands.registry`` once the registry exists.
    """
    out: set[tuple[str, str, str]] = set()
    for name in commands.cmd_list:
        cls = getattr(commands, name)
        out.add((cls.command, cls.__module__, cls.__name__))
    return frozenset(out)


def test_registry_matches_expected_surface() -> None:
    """The loader exposes exactly the snapshot commands, no more, no less."""
    actual = _current_surface()
    assert actual == EXPECTED, {
        "missing": sorted(EXPECTED - actual),
        "unexpected": sorted(actual - EXPECTED),
    }


def test_registry_count_matches_snapshot() -> None:
    """Guard against silent additions: command count is part of the contract."""
    assert len(_current_surface()) == len(EXPECTED)


def test_abstract_base_apicall_is_not_exposed() -> None:
    """``BaseApiCall`` is abstract and must not appear in the loader output."""
    class_names = {cls_name for _, _, cls_name in _current_surface()}
    assert "BaseApiCall" not in class_names
    assert "Command" not in class_names


def test_quit_aliases_are_distinct_classes() -> None:
    """``quit`` / ``exit`` / ``EOF`` are three separate classes from quit.py."""
    from mtui.commands.quit import DEOF, QExit, Quit

    by_command = {cls.command: cls for cls in (Quit, QExit, DEOF)}
    assert by_command["quit"] is Quit
    assert by_command["exit"] is QExit
    assert by_command["EOF"] is DEOF
    # exit/EOF inherit from quit; multi-class-per-module discovery must
    # keep working under __init_subclass__.
    assert issubclass(QExit, Quit)
    assert issubclass(DEOF, Quit)
