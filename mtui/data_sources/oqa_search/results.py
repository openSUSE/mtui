"""Result dataclasses (the public return shapes of the entry points)."""

from dataclasses import dataclass, field


@dataclass
class VersionResult:
    """One row in a Single Incidents / Aggregated Updates section.

    ``status`` is one of: ``"passed"``, ``"failed"``, ``"running"``,
    ``"missing"`` (no openQA build found in the date window for
    aggregated updates).
    """

    version: str
    url: str
    status: str
    failed_count: int = 0
    running_count: int = 0
    note: str = ""


@dataclass
class GroupResult:
    """Aggregated Updates results for one job group (e.g. ``core``)."""

    group: str
    versions: list[VersionResult] = field(default_factory=list)


@dataclass
class BuildCheckResult:
    """One build-check log entry parsed from qam.suse.de."""

    url: str
    matches: list[str] = field(default_factory=list)
    summary: str = ""


@dataclass
class JobResult:
    """One openQA job for an incident build.

    ``result`` is openQA's job result: ``passed``, ``softfailed``,
    ``failed``, ``parallel_failed``, ``incomplete``, ``skipped`` or
    ``obsoleted`` (superseded by a retrigger). A job that has not
    finished carries result ``none`` — its ``state`` (``scheduled``,
    ``running``, ...; ``done`` once finished) tells what it is actually
    doing. ``test`` is the scenario name (the meaningful field for
    judging relevance — unlike the full job name it does not embed the
    build string).
    """

    job_id: int
    test: str
    arch: str
    result: str
    group: str = ""
    url: str = ""
    state: str = ""
