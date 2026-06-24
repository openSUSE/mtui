"""The `list_templates` command."""

from . import Command


class ListTemplates(Command):
    """Lists all loaded templates, marking the active one.

    For each loaded template the RRID, connected host count and workflow
    mode are shown. The active template (the one plain action commands act
    on) is marked with a leading ``*``.
    """

    command = "list_templates"

    def __call__(self) -> None:
        """Executes the `list_templates` command."""
        reports = self.templates.all()
        if not reports:
            self.println("No templates loaded.")
            return

        active = self.templates.active
        for report in reports:
            marker = "*" if report is active else " "
            hosts = len(report.targets)
            mode = report.workflow.value
            self.println(f"{marker} {report.id}  hosts: {hosts}  mode: {mode}")
