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
            self.log.critical('%s: command "%s" failed:\nstdin:\n%s\nstderr:\n%s', target.hostname, stdin, stdout, stderr)
            raise UpdateError('RPM Error', target.hostname)
        if 'The following package is not supported by its vendor' in stdout:
            self.log.critical('%s: package support is uncertain:', target.hostname)
            marker = 'The following package is not supported by its vendor:\n'
            start = stdout.find(marker)
            end = stdout.find('\n\n', start)
            print(stdout[start:end])

class ZypperUpToSLE11Update(ZypperUpdate):
    def __init__(self, *a, **kw):
        super(ZypperUpToSLE11Update, self).__init__(*a, **kw)

        if not self.patches.has_key('sat'):
            self.log.critical('required SAT patch number for zypper update not found')
            return

        patch = self.patches['sat']
        self.commands = [
            'export LANG=',
            'zypper lr -puU',
            'zypper refresh',
            'zypper patches | grep " %s "' % patch,
            'for p in $(zypper patches | grep " %s " | awk \'BEGIN { FS="|"; } { print $2; }\'); do zypper -n install -l -y -t patch $p=%s; done' % (patch, patch),
        ]


class ZypperSLE12Update(ZypperUpdate):
    def __init__(self, *a, **kw):
        super(ZypperSLE12Update, self).__init__(*a, **kw)
        repo = "TESTING-{0}".format(self.testreport.rrid.maintenance_id)

        self.commands = [
            "export LANG=",
            "zypper lr -puU",
            "zypper refresh",
            "zypper patches | grep {0}".format(repo),
            "for p in $(zypper patches | grep {0} | awk 'BEGIN {{ FS=\"|\"; }} {{ print $2; }}'); do zypper -n install -l -y -t patch $p; done".format(repo),
            "zypper patches | grep {0}".format(repo)
        ]


class openSuseUpdate(Update):

    def __init__(self, *a, **kw):
        super(openSuseUpdate, self).__init__(*a, **kw)

        if not self.patches.has_key('sat'):
            self.log.critical('required SAT patch number for zypper update not found')
            return

        patch = self.patches['sat']
        self.commands = [
            'export LANG=',
            'zypper -v lr -puU',
            'zypper pch | grep " %s "' % patch,
            'zypper -v install -t patch softwaremgmt-201107=%s' % patch,
        ]

class OldZypperUpdate(Update):
    def __init__(self, *a, **kw):
        super(OldZypperUpdate, self).__init__(*a, **kw)

        if not self.patches.has_key('zypp'):
            self.log.critical('required ZYPP patch number for zypper update not found')
            return

        patch = self.patches['zypp']
        self.commands = [
            'export LANG=',
            'zypper sl',
            'zypper refresh',
            'zypper patches | grep %s-0' % patch,
            'for p in $(zypper patches | grep %s-0 | awk \'BEGIN { FS="|"; } { print $2; }\'); do zypper -n in -l -y -t patch $p; done' % patch,
        ]

class OnlineUpdate(Update):
    def __init__(self, *a, **kw):
        super(OnlineUpdate, self).__init__(*a, **kw)

        if not self.patches.has_key('you'):
            self.log.critical('required YOU patch number for online_update update not found')
            return

        patch = self.patches['you']
        self.commands = [
            'export LANG=',
            'find /var/lib/YaST2/you/ -name patch-%s' % patch,
            'online_update -V --url http://you.suse.de/download -S patch-%s -f' % patch,
            'find /var/lib/YaST2/you/ -name patch-%s' % patch,
        ]

class RugUpdate(Update):
    def __init__(self, *a, **kw):
        super(RugUpdate, self).__init__(*a, **kw)

        if not self.patches.has_key('you'):
            self.log.critical('required YOU patch number for rug update not found')
            return

        patch = self.patches['you']
        self.commands = [
            'export LANG=',
            'rug sl',
            'rug refresh',
            'rug patch-info patch-%s' % patch,
            'rug patch-install patch-%s' % patch,
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
    '10': OldZypperUpdate,
    '9': OnlineUpdate,
    'OES': RugUpdate,
    'YUM': RedHatUpdate,
}, key_error = MissingUpdaterError)


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
                commands.append('rpm -q %s &>/dev/null && zypper -n in -y -l %s %s' % (package, parameter, package))
            else:
                commands.append('zypper -n in -y -l %s %s' % (parameter, package))

        self.commands = commands

    def check(self, target, stdin, stdout, stderr, exitcode):
        if 'Error:' in stderr:
            self.log.critical('%s: command "%s" failed:\nstdin:\n%s\nstderr:\n%s', target.hostname, stdin, stdout, stderr)
            raise UpdateError(target.hostname, 'RPM Error')

class OldZypperPrepare(Prepare):

    def __init__(self, *a, **kw):
        super(OldZypperPrepare, self).__init__(*a, **kw)

        commands = []

        for package in self.packages:
            # do not install upstream-branding packages
            if 'branding-upstream' in package:
                continue
            if self.installed_only:
                commands.append('rpm -q %s &>/dev/null && zypper -n in -y -l %s' % (package, package))
            else:
                commands.append('zypper -n in -y -l %s' % package)

        self.commands = commands

class RedHatPrepare(Prepare):
    def __init__(self, *a, **kw):
        super(RedHatPrepare, self).__init__(*a, **kw)

        parameter = ''
        commands = []

        if not self.testing:
            parameter = '--disablerepo=*testing*'

        for package in self.packages:
            if self.installed_only:
                commands.append('rpm -q %s &>/dev/null && yum -y %s install %s' % (package, parameter, package))
            else:
                commands.append('yum -y %s install %s' % (parameter, package))

        self.commands = commands




Preparer = DictWithInjections({
    '11': ZypperPrepare,
    '12': ZypperPrepare,
    '114': ZypperPrepare,
    '10': OldZypperPrepare,
    'YUM': RedHatPrepare,
}, key_error = MissingPreparerError)


class ZypperDowngrade(Downgrade):
    def __init__(self, *a, **kw):
        super(ZypperDowngrade, self).__init__(*a, **kw)

        self.list_command = 'zypper se -s --match-exact -t package %s | grep -v "(System" | grep ^[iv] | sed "s, ,,g" | awk -F "|" \'{ print $2,"=",$4 }\'' % ' '.join(self.packages)
        self.install_command = 'rpm -q %s &>/dev/null && zypper -n in -C --force-resolution -y -l %s=%s'


class OldZypperDowngrade(Downgrade):
    def __init__(self, *a, **kw):
        super(OldZypperDowngrade, self).__init__(*a, **kw)

        if not self.patches.has_key('zypp'):
            self.log.critical('required ZYPP patch number for zypper downgrade not found')
            return

        patch = self.patches['zypp']
        self.list_command = 'zypper se --match-exact -t package %s | grep -v "^[iv] |[[:space:]]\+|" | grep ^[iv] | sed "s, ,,g" | awk -F "|" \'{ print $4,"=",$5 }\'' % ' '.join(self.packages)
        self.install_command = 'rpm -q %s &>/dev/null && (line=$(zypper se --match-exact -t package %s | grep %s); repo=$(zypper sl | grep "$(echo $line | cut -d \| -f 2)" | cut -d \| -f 6); if expr match "$repo" ".*/DVD1.*" &>/dev/null; then subdir="suse"; else subdir="rpm"; fi; url=$(echo -n "$repo/$subdir" | sed -e "s, ,,g" ; echo $line | awk \'{ print "/"$11"/"$7"-"$9"."$11".rpm" }\'); package=$(basename $url); if [ ! -z "$repo" ]; then wget -q $url; rpm -Uhv --nodeps --oldpackage $package; rm $package; fi)'

        commands = []

        invalid_packages = ['glibc', 'rpm', 'zypper', 'readline']
        invalid = set(self.packages).intersection(invalid_packages)
        if invalid:
            self.log.critical('crucial package found in package list: %s. please downgrade manually' % list(invalid))
            return

        commands.append('for p in $(zypper patches | grep %s-0 | awk \'BEGIN { FS="|"; } { print $2; }\'); do zypper -n rm -y -t patch $p; done'
                         % patch)

        for package in self.packages:
            commands.append('zypper -n rm -y -t atom %s' % package)

        self.post_commands = commands

class RedHatDowngrade(Downgrade):
    def __init__(self, *a, **kw):
        super(RedHatDowngrade, self).__init__(*a, **kw)
        self.commands = ['yum -y downgrade %s' % ' '.join(self.packages)]

Downgrader = DictWithInjections({
    '11': ZypperDowngrade,
    '12': ZypperDowngrade,
    '114': ZypperDowngrade,
    '10': OldZypperDowngrade,
    'YUM': RedHatDowngrade,
}, key_error = MissingDowngraderError)


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
    '10': ZypperInstall,
    'YUM': RedHatInstall,
}, key_error = MissingInstallerError)


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
    '10': ZypperUninstall,
    'YUM': RedHatUninstall,
}, key_error = MissingUninstallerError)
