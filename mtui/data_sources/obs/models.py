"""XML parsers for the OBS payloads the native QAM ops need (no osc import).

Small, explicit :mod:`xml.etree` parsers over the ``withfullhistory=1``
request document, the ``group?login`` directory, and the
``MAINT:RejectReason`` attribute envelope. The request parser exposes each
review's NESTED ``<history>`` (``withfullhistory`` puts the assignment
events inside each ``<review>``), which the assignment state machine in
:mod:`mtui.data_sources.obs.inference` replays.
"""

from __future__ import annotations

from dataclasses import dataclass
from xml.etree import ElementTree as ET

# IBS ignores automation groups when deciding what counts as a QAM group.
_IGNORED_QAM_GROUPS = frozenset({"qam-auto", "qam-openqa"})

# The MAINT:RejectReason attribute coordinates.
REJECT_REASON_NAMESPACE = "MAINT"
REJECT_REASON_NAME = "RejectReason"


def is_qam_group(name: str) -> bool:
    """IBS QAM-group test: starts with ``qam``, minus the automation groups."""
    return name.startswith("qam") and name not in _IGNORED_QAM_GROUPS


@dataclass(frozen=True, slots=True)
class HistoryEvent:
    """One ``<history>`` entry of a review (who did what, when)."""

    who: str
    when: str
    description: str


@dataclass(frozen=True, slots=True)
class Review:
    """One ``<review>`` of a request (a group or user review + its history)."""

    state: str
    by_group: str | None
    by_user: str | None
    history: tuple[HistoryEvent, ...]


@dataclass(frozen=True, slots=True)
class Request:
    """The parts of a request the QAM ops need."""

    reqid: str
    state: str
    src_project: str | None
    reviews: tuple[Review, ...]


def _parse_review(elem: ET.Element) -> Review:
    history = tuple(
        HistoryEvent(
            who=h.get("who", ""),
            when=h.get("when", ""),
            description=(h.findtext("description") or "").strip(),
        )
        for h in elem.findall("history")
    )
    return Review(
        state=elem.get("state", ""),
        by_group=elem.get("by_group"),
        by_user=elem.get("by_user"),
        history=history,
    )


def _parse_request_element(root: ET.Element) -> Request:
    state_el = root.find("state")
    source_el = root.find("action/source")
    return Request(
        reqid=root.get("id", ""),
        state=state_el.get("name", "") if state_el is not None else "",
        src_project=source_el.get("project") if source_el is not None else None,
        reviews=tuple(_parse_review(r) for r in root.findall("review")),
    )


def parse_request(xml: str) -> Request:
    """Parse a ``request?withfullhistory=1`` document."""
    return _parse_request_element(ET.fromstring(xml))


def parse_request_collection(xml: str) -> list[Request]:
    """Parse a ``<collection>`` of requests (the previous-reject search)."""
    root = ET.fromstring(xml)
    return [_parse_request_element(r) for r in root.findall("request")]


def parse_group_directory(xml: str) -> list[str]:
    """Parse a ``group?login=<user>`` directory into its group names."""
    root = ET.fromstring(xml)
    return [name for e in root.findall("entry") if (name := e.get("name"))]


def parse_reject_reason_values(xml: str) -> list[str]:
    """Parse the ``MAINT:RejectReason`` attribute doc into its value list.

    Tolerates the empty ``<attributes/>`` OBS returns when the attribute is
    unset (returns ``[]``).
    """
    root = ET.fromstring(xml)
    return [v.text.strip() for v in root.iter("value") if v.text and v.text.strip()]


def build_reject_reason_body(values: list[str]) -> str:
    """Build the ``<attributes>`` POST body for ``MAINT:RejectReason``."""
    attributes = ET.Element("attributes")
    attribute = ET.SubElement(
        attributes,
        "attribute",
        {"name": REJECT_REASON_NAME, "namespace": REJECT_REASON_NAMESPACE},
    )
    for val in values:
        ET.SubElement(attribute, "value").text = val
    return ET.tostring(attributes, encoding="unicode")
