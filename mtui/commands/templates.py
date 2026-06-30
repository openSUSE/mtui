"""The `list_templates` command."""

from . import Command


class ListTemplates(Command):
    """Lists all loaded templates, marking the active one.

    For each loaded template the RRID, connected host count and workflow
    mode are shown. In the REPL the active template (the one plain action
    commands act on) is marked with a leading ``*``. Under MCP there is no
    client-addressable active pointer (``switch`` is REPL-only), so the
    marker is omitted.
    """

    command = "list_templates"

    def __call__(self) -> None:
        """Executes the `list_templates` command."""
        reports = self.templates.all()
        if not reports:
            self.println("No templates loaded.")
            return

        # The active pointer is only meaningful in the interactive REPL, where
        # ``switch`` can move it. Under MCP it is hidden state the client cannot
        # address, so do not advertise it with a marker.
        interactive = getattr(self.prompt, "interactive", True)
        active = self.templates.active
        for report in reports:
            marker = "*" if interactive and report is active else " "
            hosts = len(report.targets)
            mode = report.workflow.value
            self.println(f"{marker} {report.id}  hosts: {hosts}  mode: {mode}")
