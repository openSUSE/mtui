"""Tests for the openQA install-job log filename resolution."""

from mtui.data_sources.openqa_install import install_logfile_for

DEFAULT = "update_install-zypper.log"


def test_classic_job_uses_default():
    """A classic install job falls back to the configured default."""
    assert install_logfile_for("qam-incidentinstall", DEFAULT) == DEFAULT
    assert install_logfile_for("qam-incidentinstall-ha", DEFAULT) == DEFAULT


def test_slfo_job_uses_slfo_logfile():
    """SLFO install jobs resolve to the SLFO install log filename."""
    assert (
        install_logfile_for("qam-incidentinstall-SLFO", DEFAULT)
        == "SLFO_update_install-zypper.log"
    )


def test_slfo_marker_matches_variants():
    """The SLFO marker matches name variants by substring."""
    assert (
        install_logfile_for("qam-incidentinstall-SLFO-ha", DEFAULT)
        == "SLFO_update_install-zypper.log"
    )


def test_unknown_and_empty_names_use_default():
    """Unknown or empty job names fall back to the default."""
    assert install_logfile_for("qam-somethingelse", DEFAULT) == DEFAULT
    assert install_logfile_for("", DEFAULT) == DEFAULT
