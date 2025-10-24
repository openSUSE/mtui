import pytest
from mtui import messages

def test_messages():
    """
    Test all UserMessage subclasses
    """
    # Test messages with arguments
    assert str(messages.HostIsNotConnectedError("test_host")) == "Host 'test_host' is not connected"
    assert str(messages.SystemCommandNotFoundError("test_cmd")) == "Command 'test_cmd' not found"
    assert str(messages.SystemCommandError(1, "test_cmd")) == "Command failed. rc = 1 Command: 'test_cmd'"
    assert str(messages.SvnCheckoutInterruptedError("test_uri")) == "Svn checkout of 'test_uri' interrupted"
    assert str(messages.SvnCheckoutFailed("test_uri", "test_url")) == "Svn checkout of 'test_uri' Failed\n Please check test_url"
    assert str(messages.ConnectingTargetFailedMessage("test_host", "test_reason")) == "connecting to test_host failed: test_reason"
    assert str(messages.ConnectingToMessage("test_host")) == "connecting to test_host"
    assert str(messages.FailedToWriteScriptResult("test_path", "test_reason")) == "failed to write script output to test_path: test_reason"
    assert str(messages.StartingCompareScriptError("test_reason", "test_argv")) == "Starting compare script 'test_argv' failed: test_reason"
    assert str(messages.CompareScriptFailed("test_argv", "stdout", "stderr", 1)) == "Compare script 'test_argv' failed: rc = 1 err:\nstderr"
    assert str(messages.CompareScriptCrashed("test_argv", "stdout", "stderr", 1)) == "Compare script 'test_argv' crashed:\nstderr"
    assert str(messages.LocationChangedMessage("old", "new")) == "changed location from 'old' to 'new'"
    assert str(messages.PackageRevisionHasntChangedWarning("test_pkg")) == "Revision of package 'test_pkg' hasn't changed, it's most likely already updated. skipping."
    assert str(messages.MissingPreparerError("test_release")) == "Missing Preparer for test_release"
    assert str(messages.MissingUpdaterError("test_release")) == "Missing Updater for test_release"
    assert str(messages.MissingInstallerError("test_release")) == "Missing Installer for test_release"
    assert str(messages.MissingUninstallerError("test_release")) == "Missing Uninstaller for test_release"
    assert str(messages.MissingDowngraderError("test_release")) == "Missing Downgrader for test_release"
    assert str(messages.InvalidLocationError("req", ["avail"])) == "Invalid location 'req'. Available locations: avail"
    assert str(messages.ReConnectFailed("test_host")) == "Failed to re-connect to test_host"
    assert str(messages.RepositoryError("test_repo")) == "Repository empty test_repo"
    assert str(messages.ResultsMissingError("test_test", "test_arch")) == "Test: test_test on arch: test_arch missing results.json file. Please restart it."
    assert str(messages.SVNError("test_cmd")) == "SVN test_cmd command failed"

    # Test messages with no arguments
    assert str(messages.NoRefhostsDefinedError()) == "No refhosts defined"
    assert str(messages.UnexpectedlyFastCleanExitFromXdgOpen()) == "xdg-open finished successfully but suspiciously too fast"
    assert str(messages.QadbReportCommentLengthWarning()) == "comment strings > 100 chars are truncated by remote_qa_db_report.pl"
    assert str(messages.MissingPackagesError()) == "Missing packages: TestReport not loaded and no -p given."
    assert str(messages.TestReportNotLoadedError()) == "TestReport not loaded"
    assert str(messages.MetadataNotLoadedError()) == "Metadata not found"
    assert str(messages.openQAError()) == "Something wrong with openQA connection"
    assert str(messages.SMELTError()) == "Sommething wrong with SMELT connection"
