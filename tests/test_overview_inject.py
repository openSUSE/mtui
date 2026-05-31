"""Tests for the renderer and the testreport injector."""

from __future__ import annotations

from mtui.data_sources.oqa_search import (
    OVERVIEW_BEGIN_MARKER,
    OVERVIEW_END_MARKER,
    BuildCheckResult,
    GroupResult,
    VersionResult,
    render_overview,
)
from mtui.update_workflow.export.overview_inject import inject_overview


def _template_with_regression_section() -> list[str]:
    """A minimal testreport-shaped template with the regression section."""
    return [
        "Maintenance Test Update Installer\n",
        "\n",
        "Some preamble line.\n",
        "\n",
        "regression tests:\n",
        "-----------------\n",
        "\n",
        "(put your details here)\n",
        "\n",
        "build log review:\n",
        "-----------------\n",
        "\n",
        "TEST_SUITE_PRESENT: NO\n",
    ]


def _sample_overview() -> tuple[
    list[VersionResult], list[GroupResult], list[BuildCheckResult]
]:
    single = [
        VersionResult(version="15-SP5", url="https://oqa/u1", status="passed"),
        VersionResult(
            version="15-SP4",
            url="https://oqa/u2",
            status="failed",
            failed_count=3,
        ),
    ]
    aggregated = [
        GroupResult(
            group="core",
            versions=[
                VersionResult(version="15-SP5", url="https://oqa/agg", status="passed")
            ],
        )
    ]
    build_checks = [
        BuildCheckResult(
            url="https://qam/xz.x86_64.log",
            matches=["[   28s] All 9 tests passed"],
        )
    ]
    return single, aggregated, build_checks


# --- renderer ---


def test_render_overview_includes_all_sections_with_headers():
    single, aggregated, build_checks = _sample_overview()
    lines = render_overview(single, aggregated, build_checks)

    text = "\n".join(lines)
    assert "## OpenQA Overview" in text
    assert "### Single Incidents - Core" in text
    assert "### Aggregated Updates - Core" in text
    assert "### Build Checks" in text
    # Per-version status text survives.
    assert "PASSED" in text
    assert "FAILED (3 jobs)" in text
    # Aggregated row URL preserved.
    assert "https://oqa/agg" in text
    # OBS timestamp stripped from build-check line.
    assert "[   28s]" not in text
    assert "All 9 tests passed" in text


def test_render_overview_skip_aggregated_hides_section():
    single, aggregated, build_checks = _sample_overview()
    lines = render_overview(single, aggregated, build_checks, skip_aggregated=True)
    assert not any("Aggregated Updates" in line for line in lines)
    # Single + build still rendered.
    assert any("Single Incidents - Core" in line for line in lines)
    assert any("Build Checks" in line for line in lines)


def test_render_overview_no_data_renders_empty_placeholders():
    lines = render_overview([], [], [])
    text = "\n".join(lines)
    assert "_No openQA builds for this incident yet._" in text
    assert "_No build checks for this incident._" in text


def test_render_overview_skip_aggregated_with_empty_single_shows_hint():
    """Regression: --no-aggregated with no single-incident results must
    still emit the "No openQA builds" hint instead of silently
    rendering only the Build Checks section.
    """
    lines = render_overview([], [], [], skip_aggregated=True)
    text = "\n".join(lines)
    assert "_No openQA builds for this incident yet._" in text
    # Aggregated section stays hidden because of skip_aggregated.
    assert not any("Aggregated Updates" in line for line in lines)


# --- inject_overview ---


def test_inject_overview_inserts_block_under_regression_section():
    template = _template_with_regression_section()
    single, aggregated, build_checks = _sample_overview()

    modified = inject_overview(template, single, aggregated, build_checks)

    assert modified is True
    body = "".join(template)
    # Block markers present.
    assert OVERVIEW_BEGIN_MARKER in body
    assert OVERVIEW_END_MARKER in body
    # Content present.
    assert "## OpenQA Overview" in body
    assert "FAILED (3 jobs)" in body
    # Block is BEFORE the next section header.
    begin_idx = body.index(OVERVIEW_BEGIN_MARKER)
    end_idx = body.index(OVERVIEW_END_MARKER)
    next_section_idx = body.index("build log review:")
    assert begin_idx < end_idx < next_section_idx
    # And AFTER the regression-tests header line.
    regression_idx = body.index("regression tests:")
    assert regression_idx < begin_idx


def test_inject_overview_preserves_existing_user_text_in_section():
    template = _template_with_regression_section()
    # Replace the placeholder with user-typed text.
    placeholder_idx = template.index("(put your details here)\n")
    template[placeholder_idx] = "My manual notes go here.\n"

    single, aggregated, build_checks = _sample_overview()
    inject_overview(template, single, aggregated, build_checks)

    body = "".join(template)
    assert "My manual notes go here." in body
    # And our block came after the user notes.
    user_idx = body.index("My manual notes go here.")
    begin_idx = body.index(OVERVIEW_BEGIN_MARKER)
    assert user_idx < begin_idx


def test_inject_overview_idempotent_on_reexport():
    """Re-injecting replaces the prior block instead of duplicating."""
    template = _template_with_regression_section()
    single, aggregated, build_checks = _sample_overview()

    inject_overview(template, single, aggregated, build_checks)
    first_pass = "".join(template)
    assert first_pass.count(OVERVIEW_BEGIN_MARKER) == 1

    # Mutate the data and re-inject.
    single2 = [VersionResult(version="12-SP5", url="u_new", status="passed")]
    inject_overview(template, single2, [], [])
    second_pass = "".join(template)

    # Still exactly one block.
    assert second_pass.count(OVERVIEW_BEGIN_MARKER) == 1
    assert second_pass.count(OVERVIEW_END_MARKER) == 1
    # New data is in, old data is gone.
    assert "12-SP5" in second_pass
    assert "15-SP4" not in second_pass


def test_inject_overview_returns_false_when_no_regression_section():
    """Templates without `regression tests:` are left untouched."""
    template = ["A line\n", "Another line\n"]
    single, aggregated, build_checks = _sample_overview()

    modified = inject_overview(template, single, aggregated, build_checks)

    assert modified is False
    assert template == ["A line\n", "Another line\n"]


def test_inject_overview_works_when_no_next_section_header():
    """If `build log review:` is absent, the block lands at end of file."""
    template = [
        "regression tests:\n",
        "-----------------\n",
        "\n",
        "(put your details here)\n",
    ]
    single, aggregated, build_checks = _sample_overview()

    modified = inject_overview(template, single, aggregated, build_checks)

    assert modified is True
    body = "".join(template)
    assert OVERVIEW_BEGIN_MARKER in body
    assert body.index("regression tests:") < body.index(OVERVIEW_BEGIN_MARKER)


def test_inject_overview_leaves_one_blank_above_block():
    """Exactly one blank line separates prior content from the begin marker."""
    template = _template_with_regression_section()
    single, aggregated, build_checks = _sample_overview()
    inject_overview(template, single, aggregated, build_checks)

    begin_idx = template.index(_BEGIN_LINE := f"{OVERVIEW_BEGIN_MARKER}\n")
    # The line immediately above the marker must be blank.
    assert template[begin_idx - 1] == "\n"
    # And the line above THAT must NOT be blank (no double-blank stacking).
    assert template[begin_idx - 2] != "\n"


def test_inject_overview_leaves_one_blank_below_block():
    """Exactly one blank line separates the end marker from the next section."""
    template = _template_with_regression_section()
    single, aggregated, build_checks = _sample_overview()
    inject_overview(template, single, aggregated, build_checks)

    end_idx = template.index(f"{OVERVIEW_END_MARKER}\n")
    # The line immediately below the end marker must be blank.
    assert template[end_idx + 1] == "\n"
    # And the line below THAT must NOT be blank.
    assert template[end_idx + 2] != "\n"


def test_inject_overview_blank_counts_stable_across_reexports():
    """Repeated re-exports do not grow the surrounding blank lines."""
    template = _template_with_regression_section()
    single, aggregated, build_checks = _sample_overview()

    inject_overview(template, single, aggregated, build_checks)
    blanks_first = sum(1 for line in template if line == "\n")

    inject_overview(template, single, aggregated, build_checks)
    blanks_second = sum(1 for line in template if line == "\n")

    inject_overview(template, single, aggregated, build_checks)
    blanks_third = sum(1 for line in template if line == "\n")

    assert blanks_first == blanks_second == blanks_third
