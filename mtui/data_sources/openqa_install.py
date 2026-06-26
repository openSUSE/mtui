"""Resolution of openQA install-test job names to their log filenames.

Different install-test scenarios publish their zypper install log under
different filenames. The classic ``qam-incidentinstall`` (and the HA
variant ``qam-incidentinstall-ha``) jobs publish ``update_install-zypper.log``
-- the value carried by the ``[openqa] install_logfile`` config option. SLFO
jobs (``qam-incidentinstall-SLFO``) instead publish
``SLFO_update_install-zypper.log``.

A single config value cannot express this per-job-name divergence, so the
URL builders in the auto connectors consult :func:`install_logfile_for`,
which maps a job name to its log filename and falls back to the configured
default for the classic case.
"""

# Map a job-name marker to the install log filename that job publishes.
# The classic jobs are intentionally absent: they fall back to the
# configured ``[openqa] install_logfile`` default via the ``default``
# argument of :func:`install_logfile_for`.
_INSTALL_LOGFILES: dict[str, str] = {
    "-SLFO": "SLFO_update_install-zypper.log",
}


def install_logfile_for(test_name: str, default: str) -> str:
    """Return the install-log filename for an openQA install-test job.

    Args:
        test_name: The openQA job/test name (e.g. ``qam-incidentinstall``
            or ``qam-incidentinstall-SLFO``).
        default: The fallback filename for jobs with no specific mapping
            (the resolved ``[openqa] install_logfile`` config value).

    Returns:
        The log filename the job publishes. Matching is by marker
        substring so name variants (e.g. ``qam-incidentinstall-SLFO-ha``)
        resolve to the same SLFO log; unknown names return ``default``.

    """
    name = test_name or ""
    for marker, logfile in _INSTALL_LOGFILES.items():
        if marker in name:
            return logfile
    return default
