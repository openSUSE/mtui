"""Mutation-killing pins for ``overview_inject`` re-export safety.

The existing idempotency tests count markers and blank lines but never
diff the non-owned lines, so mutants that eat a neighboring section
header or a tester-authored line survived. These tests pin that
re-exports leave everything outside the BEGIN/END markers intact, and
that aggregated data is rendered by default.
"""

from __future__ import annotations

from mtui.data_sources.oqa_search import (
    OVERVIEW_BEGIN_MARKER,
    OVERVIEW_END_MARKER,
    GroupResult,
    VersionResult,
)
from mtui.update_workflow.export.overview_inject import inject_overview

BEGIN_LINE = OVERVIEW_BEGIN_MARKER + "\n"
END_LINE = OVERVIEW_END_MARKER + "\n"


def _single() -> list[VersionResult]:
    return [VersionResult(version="15-SP5", url="https://oqa/u1", status="passed")]


def _aggregated() -> list[GroupResult]:
    return [
        GroupResult(
            group="core",
            versions=[
                VersionResult(version="15-SP5", url="https://oqa/agg", status="passed")
            ],
        )
    ]


def _template() -> list[str]:
    return [
        "Maintenance Test Update Installer\n",
        "\n",
        "regression tests:\n",
        "-----------------\n",
        "\n",
        "(put your details here)\n",
        "\n",
        "build log review:\n",
        "-----------------\n",
    ]


def test_inject_overview_reexport_is_exact_noop() -> None:
    """Re-injecting identical data reproduces the template byte for byte.

    Full-list equality (unlike marker/blank counting) catches removal
    windows that grow into non-owned lines -- e.g. eating the
    'build log review:' header that follows the block's trailing blank.
    """
    template = _template()
    inject_overview(template, _single(), _aggregated(), [])
    after_first = list(template)
    assert "build log review:\n" in after_first

    inject_overview(template, _single(), _aggregated(), [])

    assert list(template) == after_first


def test_inject_overview_keeps_user_line_directly_after_end_marker() -> None:
    """Only a BLANK line after the end marker is swallowed on re-export;
    tester text glued right below the block must survive."""
    template = [
        "regression tests:\n",
        "-----------------\n",
        "\n",
        BEGIN_LINE,
        "old row\n",
        END_LINE,
        "user note after\n",
        "\n",
        "build log review:\n",
    ]

    inject_overview(template, _single(), [], [])

    assert "user note after\n" in template
    assert "old row\n" not in template
    assert "build log review:\n" in template


def test_inject_overview_keeps_user_line_directly_above_begin_marker() -> None:
    """The leading-blank swallow must not fire when the line above the
    begin marker is tester text."""
    template = [
        "regression tests:\n",
        "user note above\n",
        BEGIN_LINE,
        "old row\n",
        END_LINE,
        "\n",
        "build log review:\n",
    ]

    inject_overview(template, _single(), [], [])

    assert "user note above\n" in template
    assert "old row\n" not in template


def test_inject_overview_renders_aggregated_rows_by_default() -> None:
    """Without skip_aggregated the aggregated section reaches the report."""
    template = _template()

    inject_overview(template, _single(), _aggregated(), [])

    body = "".join(template)
    assert "Aggregated Updates" in body
    assert "https://oqa/agg" in body


def test_inject_overview_no_next_header_keeps_single_gap() -> None:
    """File ends on a blank with no next-section header: the block lands
    after it without stacking a second blank above the begin marker."""
    template = [
        "regression tests:\n",
        "-----------------\n",
        "notes\n",
        "\n",
    ]

    inject_overview(template, _single(), [], [])

    begin = template.index(BEGIN_LINE)
    assert template[begin - 1] == "\n"
    assert template[begin - 2] != "\n"
