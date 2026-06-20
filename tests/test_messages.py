from mtui.support import messages


def test_messages():
    """Test all UserMessage subclasses."""
    # Test messages with arguments
    assert (
        str(messages.HostIsNotConnectedError("test_host"))
        == "Host 'test_host' is not connected"
    )
    assert (
        str(messages.SystemCommandNotFoundError("test_cmd"))
        == "Command 'test_cmd' not found"
    )
    assert (
        str(messages.SystemCommandError(1, "test_cmd"))
        == "Command failed. rc = 1 Command: 'test_cmd'"
    )
    assert (
        str(messages.SvnCheckoutInterruptedError("test_uri"))
        == "Svn checkout of 'test_uri' interrupted"
    )
    assert (
        str(messages.SvnCheckoutFailed("SUSE:SLFO:1.2:9999", "test_url"))
        == "Test report for SUSE:SLFO:1.2:9999 does not exist.\n"
        "Please check test_url for potential issues."
    )
    assert (
        str(messages.ConnectingTargetFailedMessage("test_host", "test_reason"))
        == "connecting to test_host failed: test_reason"
    )
    assert (
        str(messages.FailedToWriteScriptResult("test_path", "test_reason"))
        == "failed to write script output to test_path: test_reason"
    )
    assert (
        str(messages.StartingCompareScriptError("test_reason", "test_argv"))
        == "Starting compare script 'test_argv' failed: test_reason"
    )
    assert (
        str(messages.CompareScriptFailedError("test_argv", "stdout", "stderr", 1))
        == "Compare script 'test_argv' failed: rc = 1 err:\nstderr"
    )
    assert (
        str(messages.CompareScriptCrashedError("test_argv", "stdout", "stderr", 1))
        == "Compare script 'test_argv' crashed:\nstderr"
    )
    assert (
        str(messages.LocationChangedMessage("old", "new"))
        == "changed location from 'old' to 'new'"
    )
    assert (
        str(messages.MissingPreparerError("test_release"))
        == "Missing Preparer for test_release"
    )
    assert (
        str(messages.MissingUpdaterError("test_release"))
        == "Missing Updater for test_release"
    )
    assert (
        str(messages.MissingInstallerError("test_release"))
        == "Missing Installer for test_release"
    )
    assert (
        str(messages.MissingUninstallerError("test_release"))
        == "Missing Uninstaller for test_release"
    )
    assert (
        str(messages.MissingDowngraderError("test_release"))
        == "Missing Downgrader for test_release"
    )
    assert (
        str(messages.InvalidLocationError("req", ["avail"]))
        == "Invalid location 'req'. Available locations: avail"
    )
    assert (
        str(messages.ReConnectFailed("test_host"))
        == "Failed to re-connect to test_host"
    )
    assert (
        str(messages.ResultsMissingError("test_test", "test_arch"))
        == "Test: test_test on arch: test_arch missing results.json file. Please restart it."
    )

    # Test messages with no arguments
    assert str(messages.NoRefhostsDefinedError()) == "No refhosts defined"
    assert (
        str(messages.MissingPackagesError())
        == "Missing packages: TestReport not loaded and no -p given."
    )
    assert str(messages.TestReportNotLoadedError()) == "TestReport not loaded"
    assert str(messages.MetadataNotLoadedError()) == "Metadata not found"


def test_usermessage_is_hashable_and_consistent_with_eq():
    """Hashing a UserMessage must not recurse and must agree with __eq__."""
    a = messages.HostIsNotConnectedError("h")
    b = messages.HostIsNotConnectedError("h")
    # Previously __hash__ returned hash(self) -> RecursionError.
    assert hash(a) == hash(str(a))
    # __eq__ compares on str(self); equal objects must hash equal so they
    # behave in sets/dicts.
    assert a == b
    assert len({a, b}) == 1
