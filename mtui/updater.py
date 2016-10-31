# -*- coding: utf-8 -*-
#
# update and software stack management
#

from __future__ import print_function

from mtui.target import *
from mtui.target.actions import UpdateError
from mtui.target.downgrade import Downgrade
from mtui.target.install import Install
from mtui.target.prepare import Prepare
from mtui.target.update import Update

from mtui.messages import MissingPreparerError
from mtui.messages import MissingUpdaterError
from mtui.messages import MissingInstallerError
from mtui.messages import MissingUninstallerError
from mtui.messages import MissingDowngraderError


class ZypperUpdate(Update):

    def check(self, target, stdin, stdout, stderr, exitcode):
        if 'Error:' in stderr:
            self.log.critical(
                '%s: command "%s" failed:\nstdin:\n%s\nstderr:\n%s',
                target.hostname,
                stdin,
                stdout,
                stderr)
            raise UpdateError('RPM Error', target.hostname)
        if 'The following package is not supported by its vendor' in stdout:
            self.log.critical(
                '%s: package support is uncertain:',
                target.hostname)
            marker = 'The following package is not supported by its vendor:\n'
            start = stdout.find(marker)
            end = stdout.find('\n\n', start)
            print(stdout[start:end])


class ZypperUpToSLE11Update(ZypperUpdate):

    def __init__(self, *a, **kw):
        super(ZypperUpToSLE11Update, self).__init__(*a, **kw)

        if 'sat' not in self.patches:
            self.log.critical(
                'required SAT patch number for zypper update not found')
            return

        patch = self.patches['sat']
        self.commands = [
            r"""export LANG=""",
            r"""zypper lr -puU""",
            r"""zypper refresh""",
            r"""zypper patches | grep ' %s '""" %
            patch,
            r"""zypper patches | awk -F '|' '/ %s / { print $2; }' | while read p; do zypper -n install -l -y -t patch $p=%s; done""" %
            (patch,
             patch),
            ]


class ZypperSLE12Update(ZypperUpdate):

    def __init__(self, *a, **kw):
        super(ZypperSLE12Update, self).__init__(*a, **kw)
        repat = ':p=%d'
        repo = repat % (self.testreport.rrid.maintenance_id)

        self.commands = [
            r"""export LANG=""",
            r"""zypper lr -puU""",
            r"""zypper refresh""",
            r"""zypper patches | grep %s""" %
            repo,
            r"""zypper patches | awk -F "|" '/%s\>/ { print $2; }' | while read p; do zypper -n install -l -y -t patch $p; done""" %
            repo,
            r"""zypper patches | grep %s""" %
            repo,
            r"""zypper lr | awk -F "|" '/%s\>/ { print $2; }' | while read r; do zypper rr $r; done""" %
            repo,
            ]


class openSuseUpdate(Update):

    def __init__(self, *a, **kw):
        super(openSuseUpdate, self).__init__(*a, **kw)

        if 'sat' not in self.patches:
            self.log.critical(
                'required SAT patch number for zypper update not found')
            return

        patch = self.patches['sat']
        self.commands = [
            'export LANG=',
            'zypper -v lr -puU',
            'zypper pch | grep " %s "' % patch,
            'zypper -v install -t patch softwaremgmt-201107=%s' % patch,
        ]


class RedHatUpdate(Update):

    def __init__(self, *a, **kw):
        super(RedHatUpdate, self).__init__(*a, **kw)

        self.commands = [
            'export LANG=',
            'yum repolist',
            'yum -y update %s' % ' '.join(self.packages),
        ]

Updater = DictWithInjections({
    '11': ZypperUpToSLE11Update,
    '12': ZypperSLE12Update,
    '114': openSuseUpdate,
    'YUM': RedHatUpdate,
}, key_error=MissingUpdaterError)


class ZypperPrepare(Prepare):

    def __init__(self, *a, **kw):
        super(ZypperPrepare, self).__init__(*a, **kw)

        parameter = ''
        commands = []

        if self.force:
            parameter = '--force-resolution'

        for package in self.packages:
            if 'branding-upstream' in package:
                continue
            if self.installed_only:
                commands.append(
                    'rpm -q %s &>/dev/null && zypper -n in -y -l %s %s' %
                    (package, parameter, package))
            else:
                commands.append(
                    'zypper -n in -y -l %s %s' %
                    (parameter, package))

        self.commands = commands

    def check(self, target, stdin, stdout, stderr, exitcode):
        if 'Error:' in stderr:
            self.log.critical(
                '%s: command "%s" failed:\nstdin:\n%s\nstderr:\n%s',
                target.hostname,
                stdin,
                stdout,
                stderr)
            raise UpdateError(target.hostname, 'RPM Error')


class RedHatPrepare(Prepare):

    def __init__(self, *a, **kw):
        super(RedHatPrepare, self).__init__(*a, **kw)

        parameter = ''
        commands = []

        if not self.testing:
            parameter = '--disablerepo=*testing*'

        for package in self.packages:
            if self.installed_only:
                commands.append(
                    'rpm -q %s &>/dev/null && yum -y %s install %s' %
                    (package, parameter, package))
            else:
                commands.append('yum -y %s install %s' % (parameter, package))

        self.commands = commands


Preparer = DictWithInjections({
    '11': ZypperPrepare,
    '12': ZypperPrepare,
    '114': ZypperPrepare,
    'YUM': RedHatPrepare,
}, key_error=MissingPreparerError)


class ZypperDowngrade(Downgrade):

    def __init__(self, *a, **kw):
        super(ZypperDowngrade, self).__init__(*a, **kw)

        self.list_command = r'''
            for p in %s; do \
              zypper se -s --match-exact -t package $p; \
            done \
            | grep -v "(System" \
            | grep ^[iv] \
            | sed "s, ,,g" \
            | awk -F "|" '{ print $2,"=",$4 }'
        ''' % ' '.join(self.packages)
        self.install_command = 'rpm -q %s &>/dev/null && zypper -n in -C --force-resolution -y -l %s=%s'


class RedHatDowngrade(Downgrade):

    def __init__(self, *a, **kw):
        super(RedHatDowngrade, self).__init__(*a, **kw)
        self.commands = ['yum -y downgrade %s' % ' '.join(self.packages)]

Downgrader = DictWithInjections({
    '11': ZypperDowngrade,
    '12': ZypperDowngrade,
    '114': ZypperDowngrade,
    'YUM': RedHatDowngrade,
}, key_error=MissingDowngraderError)


class ZypperInstall(Install):

    def __init__(self, logger, targets, packages):
        Install.__init__(self, logger, targets, packages)

        commands = []

        commands.append('zypper -n in -y -l %s' % ' '.join(packages))

        self.commands = commands


class RedHatInstall(Install):

    def __init__(self, logger, targets, packages):
        Install.__init__(self, logger, targets, packages)

        commands = []

        commands.append('yum -y install %s' % ' '.join(packages))

        self.commands = commands


Installer = DictWithInjections({
    '11': ZypperInstall,
    '12': ZypperInstall,
    '114': ZypperInstall,
    'YUM': RedHatInstall,
}, key_error=MissingInstallerError)


class ZypperUninstall(Install):

    def __init__(self, logger, targets, packages):
        Install.__init__(self, logger, targets, packages)

        commands = []

        commands.append('zypper -n rm %s' % ' '.join(packages))

        self.commands = commands


class RedHatUninstall(Install):

    def __init__(self, logger, targets, packages):
        Install.__init__(self, logger, targets, packages)

        commands = []

        commands.append('yum -y remove %s' % ' '.join(packages))

        self.commands = commands


Uninstaller = DictWithInjections({
    '11': ZypperUninstall,
    '12': ZypperUninstall,
    '114': ZypperUninstall,
    'YUM': RedHatUninstall,
}, key_error=MissingUninstallerError)
