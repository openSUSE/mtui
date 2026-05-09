"""Tests for the C9 domain enums in mtui.types.enums."""

import pytest

from mtui.types import ExecutionMode, RequestKind, TargetState


class TestTargetState:
    """TargetState is a StrEnum so legacy string consumers keep working."""

    def test_members_carry_legacy_string_values(self):
        assert TargetState.ENABLED.value == "enabled"
        assert TargetState.DRYRUN.value == "dryrun"
        assert TargetState.DISABLED.value == "disabled"

    def test_str_enum_compares_equal_to_raw_string(self):
        # The whole point of StrEnum here: every existing
        # ``target.state == "enabled"`` test site keeps passing.
        assert TargetState.ENABLED == "enabled"
        assert TargetState.DRYRUN == "dryrun"
        assert TargetState.DISABLED == "disabled"

    def test_str_enum_match_case_accepts_raw_string_subject(self):
        # Confirms the production match/case in Target.run /
        # Target.query_versions still hits the right arm when an external
        # caller assigns ``target.state = "dryrun"`` (raw str).
        for raw, expected in (
            ("enabled", TargetState.ENABLED),
            ("dryrun", TargetState.DRYRUN),
            ("disabled", TargetState.DISABLED),
        ):
            match raw:
                case TargetState.ENABLED:
                    hit = TargetState.ENABLED
                case TargetState.DRYRUN:
                    hit = TargetState.DRYRUN
                case TargetState.DISABLED:
                    hit = TargetState.DISABLED
                case _:
                    hit = None
            assert hit is expected, raw

    def test_construction_from_value_returns_member(self):
        assert TargetState("enabled") is TargetState.ENABLED

    def test_construction_from_unknown_raises(self):
        with pytest.raises(ValueError, match="serial"):
            TargetState("serial")  # historical bug: never a valid state


class TestExecutionMode:
    """ExecutionMode is a plain Enum; no implicit string coercion."""

    def test_has_only_two_members(self):
        assert {m.name for m in ExecutionMode} == {"PARALLEL", "SERIAL"}

    def test_value_matches_cli_vocabulary(self):
        # set_host_state accepts "parallel"/"serial" as CLI args and
        # constructs the enum via ExecutionMode(state).
        assert ExecutionMode("parallel") is ExecutionMode.PARALLEL
        assert ExecutionMode("serial") is ExecutionMode.SERIAL

    def test_does_not_compare_equal_to_raw_string(self):
        # Plain Enum (not StrEnum): the typo-catching is exactly that
        # ExecutionMode.PARALLEL != "parallel".
        assert ExecutionMode.PARALLEL != "parallel"
        assert ExecutionMode.SERIAL != "serial"


class TestRequestKind:
    """RequestKind is a plain Enum; from_token() handles long+short forms."""

    def test_canonical_values_match_wire_format(self):
        assert RequestKind.SLFO.value == "SLFO"
        assert RequestKind.MAINTENANCE.value == "Maintenance"
        assert RequestKind.PI.value == "PI"

    @pytest.mark.parametrize(
        ("token", "expected"),
        [
            ("S", RequestKind.SLFO),
            ("SLFO", RequestKind.SLFO),
            ("M", RequestKind.MAINTENANCE),
            ("Maintenance", RequestKind.MAINTENANCE),
            ("P", RequestKind.PI),
            ("PI", RequestKind.PI),
        ],
    )
    def test_from_token_accepts_long_and_short_forms(self, token, expected):
        assert RequestKind.from_token(token) is expected

    def test_from_token_rejects_unknown(self):
        with pytest.raises(ValueError, match="unknown request kind"):
            RequestKind.from_token("SLE")  # historical typo found in fixtures

    def test_does_not_compare_equal_to_raw_string(self):
        # Same typo-catching benefit as ExecutionMode.
        assert RequestKind.SLFO != "SLFO"
