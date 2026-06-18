"""A read-only client for SMELT (the SUSE Maintenance lifecycle tool).

SMELT exposes two surfaces this client wraps:

* the **REST v2** API at ``/api/experimental/v2`` — the newer SLFO update
  workflow (``updates/{id}``, ``updates/{id}/checker-results``,
  ``updates/unreleased``, ``updates/search``); responses are wrapped in a
  ``{"status": ..., "data": ...}`` envelope.
* the **GraphQL** API at ``/graphql/`` — the classic Maintenance workflow
  (``incidents``), used to read an incident's priority and dates.

The base URL comes from ``config.smelt_url`` (``[smelt] url``); when it is unset
the client is *not configured* and callers skip SMELT lookups rather than
hardcoding a host. All methods are best-effort: a transport error logs and
returns ``None``/``[]`` so a SMELT hiccup never breaks the surrounding command.
"""

from __future__ import annotations

from logging import getLogger
from typing import Any
from urllib.parse import urlparse

import requests

from ..support.config import Config
from ..support.http import HTTP_TIMEOUT, build_session, resolve_verify
from ..types import RequestKind, RequestReviewID

logger = getLogger("mtui.connector.smelt")


def slfo_update_id(gitea_pr_url: str | None, review_id: int | str) -> str | None:
    """Build the REST v2 update id for a SLFO pull request.

    The id has the shape ``<src-host>:products:SLFO:<pr-number>``. The src
    (Gitea) host is derived from the request's ``gitea_pr`` URL so it is not
    hardcoded; returns ``None`` when no host can be determined.
    """
    host = urlparse(gitea_pr_url or "").netloc
    if not host:
        return None
    return f"{host}:products:SLFO:{review_id}"


class Smelt:
    """Best-effort read-only SMELT client."""

    def __init__(self, config: Config) -> None:
        self.base = (config.smelt_url or "").rstrip("/")
        self._verify = resolve_verify(True, config.ssl_verify)

    @property
    def configured(self) -> bool:
        """True when a SMELT URL is set (``[smelt] url``)."""
        return bool(self.base)

    # --- low-level -------------------------------------------------------- #

    def _session(self) -> requests.Session:
        return build_session(self._verify)

    def _v2(self, path: str, params: dict[str, Any] | None = None) -> Any | None:
        """GET ``/api/experimental/v2/<path>`` and unwrap the ``data`` envelope."""
        if not self.configured:
            return None
        url = f"{self.base}/api/experimental/v2/{path.lstrip('/')}"
        try:
            r = self._session().get(url, params=params, timeout=HTTP_TIMEOUT)
            r.raise_for_status()
            payload = r.json()
        except (requests.exceptions.RequestException, ValueError) as e:
            logger.debug("SMELT v2 GET %s failed: %s", path, e)
            return None
        if isinstance(payload, dict) and "data" in payload:
            return payload["data"]
        return payload

    def _graphql(self, query: str) -> dict[str, Any] | None:
        if not self.configured:
            return None
        try:
            r = self._session().post(
                f"{self.base}/graphql/", json={"query": query}, timeout=HTTP_TIMEOUT
            )
            r.raise_for_status()
            payload = r.json()
        except (requests.exceptions.RequestException, ValueError) as e:
            logger.debug("SMELT GraphQL query failed: %s", e)
            return None
        if payload.get("errors"):
            logger.debug("SMELT GraphQL errors: %s", payload["errors"])
            return None
        return payload.get("data")

    # --- REST v2 (SLFO / new) -------------------------------------------- #

    def update(self, update_id: str) -> dict[str, Any] | None:
        """Full detail for one SLFO update (incl. ``priority``, ``deadline``)."""
        d = self._v2(f"updates/{update_id}")
        return d if isinstance(d, dict) else None

    def checker_results(self, update_id: str) -> list[dict[str, Any]]:
        """Checker (build-check) result runs for one SLFO update."""
        d = self._v2(f"updates/{update_id}/checker-results")
        return d if isinstance(d, list) else []

    def unreleased(self, review_group: str | None = None) -> list[dict[str, Any]]:
        """All unreleased updates, optionally narrowed to a review group."""
        params = {"review_group": review_group} if review_group else None
        d = self._v2("updates/unreleased", params)
        return d if isinstance(d, list) else []

    def search(self, text: str) -> list[dict[str, Any]]:
        """Search updates by free text (SMELT requires >= 3 characters)."""
        d = self._v2("updates/search", {"text": text})
        return d if isinstance(d, list) else []

    # --- GraphQL (Maintenance / old) ------------------------------------- #

    def incident(self, incident_id: int | str) -> dict[str, Any] | None:
        """Read a classic Maintenance incident's priority/dates via GraphQL."""
        query = (
            "{incidents(incidentId: " + str(int(incident_id)) + "){edges{node{"
            "incidentId priority crd prd status{name} rating{name}}}}}"
        )
        data = self._graphql(query)
        if not data:
            return None
        edges = (data.get("incidents") or {}).get("edges") or []
        return edges[0]["node"] if edges else None

    # --- high-level ------------------------------------------------------- #

    def priority_deadline(
        self, rrid: RequestReviewID, gitea_pr_url: str | None = None
    ) -> tuple[int | None, str | None]:
        """Return ``(priority, deadline)`` for a request, dispatching on kind.

        SLFO uses the REST v2 update (``priority``/``deadline``); classic
        Maintenance uses the GraphQL incident (``priority``/``crd``). Anything
        else, or any lookup failure, yields ``(None, None)``.
        """
        if not self.configured:
            return None, None
        if rrid.kind is RequestKind.SLFO:
            uid = slfo_update_id(gitea_pr_url, rrid.review_id)
            data = self.update(uid) if uid else None
            if not data:
                return None, None
            return data.get("priority"), data.get("deadline")
        if rrid.kind is RequestKind.MAINTENANCE:
            node = self.incident(rrid.maintenance_id)
            if not node:
                return None, None
            return node.get("priority"), node.get("crd")
        return None, None
