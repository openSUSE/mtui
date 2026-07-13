"""Native OBS/IBS QAM review backend.

Replaces the shell-out to the external ``osc qam`` plugin with direct
OBS API calls over :mod:`requests` (mirroring :mod:`mtui.data_sources.gitea`)
and native OBS "Signature" (SSH) authentication reproduced with paramiko.
No module here imports the ``osc`` library or shells out to ``osc``.

Credentials come from the user's existing ``~/.oscrc`` (parsed natively by
:mod:`~mtui.data_sources.obs.oscrc`) — oscrc stays the single source of
truth for OBS credentials; mtui reads it and authenticates itself.

This first milestone lands the credential-reader foundation; the wire-format
signer, the auth handshake, the HTTP client, and the operation logic land in
subsequent milestones behind the ``[obs] backend`` flag (default ``plugin``).
"""

from __future__ import annotations
