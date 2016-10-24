# -*- coding: utf-8 -*-
# vim: et sw=2 sts=2

from datetime import datetime

from mtui.rpmver import RPMVersion

from mtui.utils import blue, green, red, yellow


class CommandPromptDisplay(object):

    def __init__(self, output):
        self.output = output

    def println(self, msg='', eol='\n'):
        return self.output.write(msg + eol)

    def list_bugs(self, bugs, url):
        ids = sorted(bugs.keys())

        self.println(
            'Buglist: {}/buglist.cgi?bug_id={}'.format(url, ','.join(ids)))
        for (bug, summary) in [(bug, bugs[bug]) for bug in ids]:
            self.println()
            self.println('Bug #{0:5}: {1}'.format(bug, summary))
            self.println('{}/show_bug.cgi?id={}'.format(url, bug))

    def list_history(self, hostname, system, lines):
        self.println('history from {} ({}):'.format(
            hostname,
            system
        ))
        lines.reverse()
        for line in lines:
            try:
                when = line.split(':')[0]
                who = line.split(':')[1]
                event = ':'.join(line.split(':')[2:])
            except IndexError:
                continue

            time = datetime.fromtimestamp(float(when))
            self.println('{}, {}: {}'.format(
                time.strftime('%A, %d.%m.%Y %H:%M'),
                who,
                event
            ))
        self.println()

    def list_host(self, hostname, system, state, exclusive):
        if exclusive:
            mode = 'serial'
        else:
            mode = 'parallel'

        if state == 'enabled':
            state = green('Enabled')
        elif state == 'dryrun':
            state = yellow('Dryrun')
        else:
            state = red('Disabled')

        self.println('{0:20} {1:20}: {2} ({3})'.format(
            hostname,
            '(%s)' % system,
            state,
            mode
        ))

    def list_locks(self, hostname, system, lock):
        system = '(%s)' % system
        if lock.locked:
            if lock.own():
                lockedby = 'me'
            else:
                lockedby = lock.user

            self.println(eol='', msg='{0:20} {1:20}: {2}'.format(
                hostname,
                system,
                yellow('since {} by {}'.format(lock.time(), lockedby))
            ))
            if lock.comment:
                self.println(' : {}'.format(lock.comment))
            else:
                self.println()
        else:
            self.println('{0:20} {1:20}: {2}'.format(
                hostname,
                system,
                green('not locked')
            ))

    def list_patches(self, allpatches):
        for specfile, patches in allpatches:
            self.println()
            self.println('Patches in {}:'.format(specfile))

            for pn, fn, applied in patches:
                self.println('{0:45}: {1}'.format(
                    fn,
                    green('applied') if applied else red('not applied'),
                ))

    def list_sessions(self, hostname, system, stdout):
        self.println('sessions on {} ({}):'.format(hostname, system))
        self.println(stdout)

    def list_timeout(self, hostname, system, timeout):
        self.println('{0:20} {1:20}: {2}s'.format(
            hostname,
            '(%s)' % system,
            timeout,
        ))

    def list_versions(self, targets, hosts_pvs):
        for hs, pvs in hosts_pvs.items():
            if len(hosts_pvs) > 1:
                self.println('version history from:')
                for hn in hs:
                    self.println('  {} ({})'.format(hn, targets[hn].system))
                self.println()

            for pkg, vers in pvs:
                self.println('{}:'.format(pkg))
                indent = 0
                for ver in sorted(vers, key=RPMVersion, reverse=True):
                    self.println('  ' * indent + '-> {}'.format(ver))
                    indent = indent + 1
                self.println()

    def search_hosts(self, hostname, hosttags):
        self.println('{0:25}: {1}'.format(hostname, hosttags))

    def show_log(self, hostname, hostlog, sink):
        sink('log from %s:' % hostname)
        for cmdline, stdout, stderr, exitcode, _ in hostlog:
            sink('%s:~> %s [%s]' % (hostname, cmdline, exitcode))
            sink('stdout:')
            map(sink, stdout.split('\n'))
            sink('stderr:')
            map(sink, stderr.split('\n'))

    def testopia_list(self, url, tcid, summary, status, automated):
        if status == 'disabled':
            status = red('disabled')
        elif status == 'confirmed':
            status = green('confirmed')
        else:
            status = yellow('proposed')
        if automated == 'yes':
            automated = 'automated'
        else:
            automated = 'manual'
        self.println('{0:40}: {1} ({2})'.format(summary, status, automated))
        self.println('{}/tr_show_case.cgi?case_id={}'.format(url, tcid))
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
            effect):
        self.println('%s %s' % (blue('Testcase summary:'), summary))
        self.println('%s %s' % (
            blue('Testcase URL:'), '{}/tr_show_case.cgi?case_id={}'.format(url, case_id)))
        self.println('%s %s' % (blue('Testcase automated:'), automated))
        self.println('%s %s' % (blue('Testcase status:'), status))
        self.println('%s %s' % (blue('Testcase requirements:'), requirement))
        if setup:
            self.println(blue('Testcase setup:'))
            self.println(setup)
        if breakdown:
            self.println(blue('Testcase breakdown:'))
            self.println(breakdown)
        self.println(blue('Testcase actions:'))
        self.println(action)
        if effect:
            self.println(blue('Testcase effect:'))
            self.println(effect)

    def testsuite_list(self, hostname, system, suites):
        self.println('testsuites on {} ({}):'.format(hostname, system))
        self.println(
            '\n'.join([i for i in sorted(suites) if i.endswith('-run')]))
        self.println()

    def testsuite_run(self, hostname, exit, stdout, stderr, suitename):
        self.println(
            '{}:~> {}-testsuite [{}]'.format(hostname, suitename, exit))
        self.println(stdout)
        if stderr:
            self.println(stderr)
