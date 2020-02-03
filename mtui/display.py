from datetime import datetime

from .types.rpmver import RPMVersion
from .utils import blue, green, red, yellow


class CommandPromptDisplay:

    def __init__(self, output):
        self.output = output

    def println(self, msg="", eol="\n"):
        return self.output.write(msg + eol)

    def list_bugs(self, bugs, url):
        ids = sorted(bugs.keys())

        self.println(f'Buglist: {url}/buglist.cgi?bug_id={",".join(ids)}')
        for (bug, summary) in [(bug, bugs[bug]) for bug in ids]:
            self.println()
            self.println("Bug #{0:5}: {1}".format(bug, summary))
            self.println(f"{url}/show_bug.cgi?id={bug}")

    def list_history(self, hostname, system, lines):
        self.println(f"history from {hostname} ({system}):")
        lines.reverse()
        for line in lines:
            try:
                when = line.split(":")[0]
                who = line.split(":")[1]
                event = ":".join(line.split(":")[2:])
            except IndexError:
                continue

            time = datetime.fromtimestamp(float(when))
            self.println(
                "{}, {}: {}".format(time.strftime("%A, %d.%m.%Y %H:%M"), who, event)
            )
        self.println()

    def list_host(self, hostname, system, state, exclusive):
        if exclusive:
            mode = "serial"
        else:
            mode = "parallel"

        if state == "enabled":
            state = green("Enabled")
        elif state == "dryrun":
            state = yellow("Dryrun")
        else:
            state = red("Disabled")

        self.println(
            "{0:20} {1:20}: {2} ({3})".format(
                hostname, "({!s})".format(system), state, mode
            )
        )

    def list_locks(self, hostname, system, lock):
        system = "({!s})".format(system)
        if lock.is_locked():
            lockedby = "me" if lock.is_mine() else lock.locked_by()

            self.println(
                eol="",
                msg="{0:20} {1:20}: {2}".format(
                    hostname,
                    system,
                    yellow("since {} by {}".format(lock.time(), lockedby)),
                ),
            )

            # TODO: walrus operator in python 3.8 .....
            comment = lock.comment()
            if comment:
                self.println(" : {}".format(comment))
            else:
                self.println()
        else:
            self.println(
                "{0:20} {1:20}: {2}".format(hostname, system, green("not locked"))
            )

    def list_sessions(self, hostname, system, stdout):
        self.println("sessions on {} ({}):".format(hostname, system))
        self.println(stdout)

    def list_timeout(self, hostname, system, timeout):
        self.println(
            "{0:20} {1:20}: {2}s".format(hostname, "({!s})".format(system), timeout)
        )

    def list_versions(self, targets, hosts_pvs):
        for hs, pvs in list(hosts_pvs.items()):
            if len(hosts_pvs) > 1:
                self.println("version history from:")
                for hn in hs:
                    self.println("  {} ({})".format(hn, targets[hn].system))
                self.println()

            for pkg, vers in pvs:
                self.println("{}:".format(pkg))
                indent = 0
                for ver in sorted(vers, key=RPMVersion, reverse=True):
                    self.println("  " * indent + "-> {}".format(ver))
                    indent = indent + 1
                self.println()

    def list_products(self, hostname, system):
        self.println("{}: {}".format(green("Referenece host"), yellow(hostname)))
        for x in system.pretty():
            self.println(x)
        self.println()

    def list_update_repos(self, repos, update_id):
        server_update = "http://download.suse.de/ibs/" + ":/".join(
            str(update_id).split(":")[0:-1]
        )

        for p, r in repos.items():
            self.println(
                "{}: {} - {}: {} - {}: {}".format(
                    green("Product"),
                    yellow(p.name),
                    green("version"),
                    yellow(p.version),
                    green("arch"),
                    yellow(p.arch),
                )
            )
            self.println("    {}".format(server_update + "/" + r))

    @staticmethod
    def show_log(hostname, hostlog, sink):
        sink("log from {!s}:".format(hostname))
        for cmdline, stdout, stderr, exitcode, _ in hostlog:
            sink("{!s}:~> {!s} [{!s}]".format(hostname, cmdline, exitcode))
            sink("stdout:")
            list(map(sink, stdout.split("\n")))
            sink("stderr:")
            list(map(sink, stderr.split("\n")))

    def testopia_list(self, url, tcid, summary, status, automated):
        if status == "disabled":
            status = red("disabled")
        elif status == "confirmed":
            status = green("confirmed")
        else:
            status = yellow("proposed")
        if automated == "yes":
            automated = "automated"
        else:
            automated = "manual"
        self.println("{0:40}: {1} ({2})".format(summary, status, automated))
        self.println("{}/tr_show_case.cgi?case_id={}".format(url, tcid))
        self.println()

    def testopia_show(
        self,
        url,
        case_id,
        summary,
        status,
        automated,
        requirement,
        setup,
        action,
        breakdown,
        effect,
    ):
        self.println("{!s} {!s}".format(blue("Testcase summary:"), summary))
        self.println(
            "{!s} {!s}".format(
                blue("Testcase URL:"), f"{url}/tr_show_case.cgi?case_id={case_id}"
            )
        )
        self.println("{!s} {!s}".format(blue("Testcase automated:"), automated))
        self.println("{!s} {!s}".format(blue("Testcase status:"), status))
        self.println("{!s} {!s}".format(blue("Testcase requirements:"), requirement))
        if setup:
            self.println(blue("Testcase setup:"))
            self.println(setup)
        if breakdown:
            self.println(blue("Testcase breakdown:"))
            self.println(breakdown)
        self.println(blue("Testcase actions:"))
        self.println(action)
        if effect:
            self.println(blue("Testcase effect:"))
            self.println(effect)

    def testsuite_list(self, hostname, system, suites):
        self.println(f"testsuites on {hostname} ({system}):")
        self.println("\n".join([i for i in sorted(suites) if i.endswith("-run")]))
        self.println()

    def testsuite_run(self, hostname, exit, stdout, stderr, suitename):
        self.println(f"{hostname}:~> {suitename} - testsuite [{exit}]")
        self.println(stdout)
        if stderr:
            self.println(stderr)
