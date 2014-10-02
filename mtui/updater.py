# -*- coding: utf-8 -*-
#
# update and software stack management
#

from __future__ import print_function

import os
import sys
import time
import re

from mtui.target import *
from mtui.rpmver import *
from mtui.utils import *

class UpdateError(Exception):

    def __init__(self, reason, host=None):
        self.reason = reason
        self.host = host

    def __str__(self):
        if self.host is None:
            string = self.reason
        else:
            string = '%s: %s' % (self.host, self.reason)

        return repr(string)


class Update(object):

    def __init__(self, targets, patches, packages):
        self.targets = targets
        self.patches = patches
        self.commands = []

    def run(self):
        skipped = False

        try:
            for target in self.targets:
                lock = self.targets[target].locked()
                if lock.locked and not lock.own():
                    skipped = True
                    out.warning('host %s is locked since %s by %s. skipping.' % (target, lock.time(), lock.user))
                    if lock.comment:
                        out.info("%s's comment: %s" % (lock.user, lock.comment))
                else:
                    self.targets[target].set_locked()
                    thread = ThreadedMethod(queue)
                    thread.setDaemon(True)
                    thread.start()

            if skipped:
                for target in self.targets:
                    try:
                        self.targets[target].remove_lock()
                    except AssertionError:
                        pass
                raise UpdateError('Hosts locked')

            for target in self.targets:
                queue.put([self.targets[target].set_repo, ['TESTING']])

            while queue.unfinished_tasks:
                spinner()

            queue.join()

            for command in self.commands:
                RunCommand(self.targets, command).run()

                for target in self.targets:
                    self._check(self.targets[target], self.targets[target].lastin(), self.targets[target].lastout(),
                                self.targets[target].lasterr(), self.targets[target].lastexit())
        except:
            raise
        finally:
            for target in self.targets:
                if not lock.locked:  # wasn't locked earlier by set_host_lock
                    try:
                        self.targets[target].remove_lock()
                    except AssertionError:
                        pass

    def _check(self, target, stdin, stdout, stderr, exitcode):
        if 'zypper' in stdin and exitcode == 104:
            out.critical('%s: command "%s" failed:\nstdin:\n%s\nstderr:\n%s', target.hostname, stdin, stdout, stderr)
            raise UpdateError('update stack locked', target.hostname)
        if 'Additional rpm output' in stdout:
            out.warning('There was additional rpm output on %s:', target.hostname)
            marker = 'Additional rpm output:'
            start = stdout.find(marker) + len(marker)
            end = stdout.find('Retrieving', start)
            print(stdout[start:end].replace('warning', yellow('warning')))
        if 'A ZYpp transaction is already in progress.' in stderr:
            out.critical('%s: command "%s" failed:\nstdin:\n%s\nstderr:\n%s', target.hostname, stdin, stdout, stderr)
            raise UpdateError('update stack locked', target.hostname)
        if 'System management is locked' in stderr:
            out.critical('%s: command "%s" failed:\nstdin:\n%s\nstderr:\n%s', target.hostname, stdin, stdout, stderr)
            raise UpdateError('update stack locked', target.hostname)
        if '(c): c' in stdout:
            out.critical('%s: unresolved dependency problem. please resolve manually:\n%s', target.hostname, stdout)
            raise UpdateError('Dependency Error', target.hostname)

        return self.check(target, stdin, stdout, stderr, exitcode)

    def check(self, target, stdin, stdout, stderr, exitcode):
        """stub. needs to be overwritten by inherited classes"""

        return


class ZypperUpdate(Update):

    def __init__(self, targets, patches, packages):
        Update.__init__(self, targets, patches, packages)

        try:
            patch = patches['sat']
        except KeyError:
            out.critical('required SAT patch number for zypper update not found')
            return

        commands = []

        commands.append('export LANG=')
        commands.append('zypper lr -puU')
        commands.append('zypper refresh')
        commands.append('zypper patches | grep " %s "' % patch)
        commands.append('for p in $(zypper patches | grep " %s " | awk \'BEGIN { FS="|"; } { print $2; }\'); do zypper -n install -l -y -t patch $p=%s; done'
                         % (patch, patch))

        self.commands = commands

    def check(self, target, stdin, stdout, stderr, exitcode):
        if 'Error:' in stderr:
            out.critical('%s: command "%s" failed:\nstdin:\n%s\nstderr:\n%s', target.hostname, stdin, stdout, stderr)
            raise UpdateError('RPM Error', target.hostname)
        if 'The following package is not supported by its vendor' in stdout:
            out.critical('%s: package support is uncertain:', target.hostname)
            marker = 'The following package is not supported by its vendor:\n'
            start = stdout.find(marker)
            end = stdout.find('\n\n', start)
            print(stdout[start:end])


class openSuseUpdate(Update):

    def __init__(self, targets, patches, packages):
        Update.__init__(self, targets, patches, packages)

        try:
            patch = patches['sat']
        except KeyError:
            out.critical('required SAT patch number for zypper update not found')
            return

        commands = []

        commands.append('export LANG=')
        commands.append('zypper -v lr -puU')
        commands.append('zypper pch | grep " %s "' % patch)
        commands.append('zypper -v install -t patch softwaremgmt-201107=%s' % patch)

        self.commands = commands


class OldZypperUpdate(Update):

    def __init__(self, targets, patches, packages):
        Update.__init__(self, targets, patches, packages)

        try:
            patch = patches['zypp']
        except KeyError:
            out.critical('required ZYPP patch number for zypper update not found')
            return

        commands = []

        commands.append('export LANG=')
        commands.append('zypper sl')
        commands.append('zypper refresh')
        commands.append('zypper patches | grep %s-0' % patch)
        commands.append('for p in $(zypper patches | grep %s-0 | awk \'BEGIN { FS="|"; } { print $2; }\'); do zypper -n in -l -y -t patch $p; done'
                         % patch)

        self.commands = commands


class OnlineUpdate(Update):

    def __init__(self, targets, patches, packages):
        Update.__init__(self, targets, patches, packages)

        try:
            patch = patches['you']
        except KeyError:
            out.critical('required YOU patch number for online_update update not found')
            return

        commands = []

        commands.append('export LANG=')
        commands.append('find /var/lib/YaST2/you/ -name patch-%s' % patch)
        commands.append('online_update -V --url http://you.suse.de/download -S patch-%s -f' % patch)
        commands.append('find /var/lib/YaST2/you/ -name patch-%s' % patch)

        self.commands = commands


class RugUpdate(Update):

    def __init__(self, targets, patches, packages):
        Update.__init__(self, targets, patches, packages)

        try:
            patch = patches['you']
        except KeyError:
            out.critical('required YOU patch number for rug update not found')
            return

        commands = []

        commands.append('export LANG=')
        commands.append('rug sl')
        commands.append('rug refresh')
        commands.append('rug patch-info patch-%s' % patch)
        commands.append('rug patch-install patch-%s' % patch)

        self.commands = commands


class RedHatUpdate(Update):

    def __init__(self, targets, patches, packages):
        Update.__init__(self, targets, patches, packages)

        commands = []

        commands.append('export LANG=')
        commands.append('yum repolist')
        commands.append('yum -y update %s' % ' '.join(packages))

        self.commands = commands


Updater = {
    '11': ZypperUpdate,
    '114': openSuseUpdate,
    '10': OldZypperUpdate,
    '9': OnlineUpdate,
    'OES': RugUpdate,
    'YUM': RedHatUpdate,
}


class Prepare(object):

    def __init__(self, targets, testing):
        self.targets = targets
        self.testing = testing
        self.commands = []

    def run(self):
        skipped = False

        try:
            for target in self.targets:
                lock = self.targets[target].locked()
                if lock.locked and not lock.own():
                    skipped = True
                    out.warning('host %s is locked since %s by %s. skipping.' % (target, lock.time(), lock.user))
                    if lock.comment:
                        out.info("%s's comment: %s" % (lock.user, lock.comment))
                else:
                    self.targets[target].set_locked()
                    thread = ThreadedMethod(queue)
                    thread.setDaemon(True)
                    thread.start()

            if skipped:
                for target in self.targets:
                    try:
                        self.targets[target].remove_lock()
                    except AssertionError:
                        pass
                raise UpdateError('Hosts locked')

            for target in self.targets:
                if self.testing:
                    queue.put([self.targets[target].set_repo, ['TESTING']])
                else:
                    queue.put([self.targets[target].set_repo, ['UPDATE']])

            while queue.unfinished_tasks:
                spinner()

            queue.join()

            for target in self.targets:
                if self.targets[target].lasterr():
                    out.critical('failed to prepare host %s. stopping.\n# %s\n%s' % (target, self.targets[target].lastin(),
                                 self.targets[target].lasterr()))
                    return

            for command in self.commands:
                RunCommand(self.targets, command).run()

                for target in self.targets:
                    self._check(self.targets[target], self.targets[target].lastin(), self.targets[target].lastout(),
                                self.targets[target].lasterr(), self.targets[target].lastexit())
        except:
            raise
        finally:
            for target in self.targets:
                if not lock.locked:  # wasn't locked earlier by set_host_lock
                    try:
                        self.targets[target].remove_lock()
                    except AssertionError:
                        pass

    def _check(self, target, stdin, stdout, stderr, exitcode):
        if 'A ZYpp transaction is already in progress.' in stderr:
            out.critical('%s: command "%s" failed:\nstdin:\n%s\nstderr:\n%s', target.hostname, stdin, stdout, stderr)
            raise UpdateError(target.hostname, 'update stack locked')
        if 'System management is locked' in stderr:
            out.critical('%s: command "%s" failed:\nstdin:\n%s\nstderr:\n%s', target.hostname, stdin, stdout, stderr)
            raise UpdateError('update stack locked', target.hostname)
        if '(c): c' in stdout:
            out.critical('%s: unresolved dependency problem. please resolve manually:\n%s', target.hostname, stdout)
            raise UpdateError('Dependency Error', target.hostname)

        return self.check(target, stdin, stdout, stderr, exitcode)

    def check(self, target, stdin, stdout, stderr, exitcode):
        """stub. needs to be overwritten by inherited classes"""

        return


class ZypperPrepare(Prepare):

    def __init__(self, targets, packages, testing=False, force=False, installed_only=False):
        Prepare.__init__(self, targets, testing)

        parameter = ''
        commands = []

        if force:
            parameter = '--force-resolution'

        for package in packages:
            if 'branding-upstream' in package:
                continue
            if installed_only:
                commands.append('rpm -q %s &>/dev/null && zypper -n in -y -l %s %s' % (package, parameter, package))
            else:
                commands.append('zypper -n in -y -l %s %s' % (parameter, package))

        self.commands = commands

    def check(self, target, stdin, stdout, stderr, exitcode):
        if 'Error:' in stderr:
            out.critical('%s: command "%s" failed:\nstdin:\n%s\nstderr:\n%s', target.hostname, stdin, stdout, stderr)
            raise UpdateError(target.hostname, 'RPM Error')


class OldZypperPrepare(Prepare):

    def __init__(self, targets, packages, testing=False, force=False, installed_only=False):
        Prepare.__init__(self, targets, testing)

        commands = []

        for package in packages:
            # do not install upstream-branding packages
            if 'branding-upstream' in package:
                continue
            if installed_only:
                commands.append('rpm -q %s &>/dev/null && zypper -n in -y -l %s' % (package, package))
            else:
                commands.append('zypper -n in -y -l %s' % package)

        self.commands = commands

class RedHatPrepare(Prepare):

    def __init__(self, targets, packages, testing=False, force=False, installed_only=False):
        Prepare.__init__(self, targets, testing)

        parameter = ''
        commands = []

        if not testing:
            parameter = '--disablerepo=*testing*'

        for package in packages:
            if installed_only:
                commands.append('rpm -q %s &>/dev/null && yum -y %s install %s' % (package, parameter, package))
            else:
                commands.append('yum -y %s install %s' % (parameter, package))

        self.commands = commands


Preparer = {
    '11': ZypperPrepare,
    '114': ZypperPrepare,
    '10': OldZypperPrepare,
    'YUM': RedHatPrepare,
}


class Downgrade(object):

    def __init__(self, targets, packages=None):
        self.targets = targets
        self.packages = packages
        self.commands = {}
        self.install_command = None
        self.list_command = None
        self.pre_commands = []
        self.post_commands = []

    def run(self):
        skipped = False
        versions = {}

        try:
            for target in self.targets:
                lock = self.targets[target].locked()
                if lock.locked and not lock.own():
                    skipped = True
                    out.warning('host %s is locked since %s by %s. skipping.' % (target, lock.time(), lock.user))
                    if lock.comment:
                        out.info("%s's comment: %s" % (lock.user, lock.comment))
                else:
                    self.targets[target].set_locked()
                    thread = ThreadedMethod(queue)
                    thread.setDaemon(True)
                    thread.start()

            if skipped:
                for target in self.targets:
                    try:
                        self.targets[target].remove_lock()
                    except AssertionError:
                        pass
                raise UpdateError('Hosts locked')

            for target in self.targets:
                queue.put([self.targets[target].set_repo, ['UPDATE']])

            while queue.unfinished_tasks:
                spinner()

            queue.join()

            for target in self.targets:
                if self.targets[target].lasterr():
                    out.critical('failed to downgrade host %s. stopping.\n# %s\n%s' % (target, self.targets[target].lastin(),
                                 self.targets[target].lasterr()))
                    return

            RunCommand(self.targets, self.list_command).run()
            for target in self.targets:
                lines = self.targets[target].lastout().split('\n')
                release = {}
                for line in lines:
                    match = re.search('(.*) = (.*)', line)
                    if match:
                        name = match.group(1)
                        version = match.group(2)
                        try:
                            release[name].append(version)
                        except KeyError:
                            release[name] = []
                            release[name].append(version)

                for name in release:
                    version = sorted(release[name], key=RPMVersion, reverse=True)[0]
                    try:
                        versions[target].update({name:version})
                    except KeyError:
                        versions[target] = {}
                        versions[target].update({name:version})

            for command in self.pre_commands:
                RunCommand(self.targets, command).run()

            for package in self.packages:
                temp = self.targets.copy()
                for target in self.targets:
                    try:
                        command = self.install_command % (package, package, versions[target][package])
                        self.commands.update({target:command})
                    except KeyError:
                        del temp[target]

                RunCommand(temp, self.commands).run()

                for target in self.targets:
                    self._check(self.targets[target], self.targets[target].lastin(), self.targets[target].lastout(),
                                self.targets[target].lasterr(), self.targets[target].lastexit())

            for command in self.post_commands:
                RunCommand(self.targets, command).run()

        except:
            raise
        finally:
            for target in self.targets:
                if not lock.locked:  # wasn't locked earlier by set_host_lock
                    try:
                        self.targets[target].remove_lock()
                    except AssertionError:
                        pass

    def _check(self, target, stdin, stdout, stderr, exitcode):
        if 'A ZYpp transaction is already in progress.' in stderr:
            out.critical('%s: command "%s" failed:\nstdin:\n%s\nstderr:\n%s', target.hostname, stdin, stdout, stderr)
            raise UpdateError(target.hostname, 'update stack locked')
        if 'System management is locked' in stderr:
            out.critical('%s: command "%s" failed:\nstdin:\n%s\nstderr:\n%s', target.hostname, stdin, stdout, stderr)
            raise UpdateError('update stack locked', target.hostname)
        if '(c): c' in stdout:
            out.critical('%s: unresolved dependency problem. please resolve manually:\n%s', target.hostname, stdout)
            raise UpdateError('Dependency Error', target.hostname)
        if exitcode == 104:
            out.critical('%s: zypper returned with errorcode 104:\n%s', target.hostname, stderr)
            raise UpdateError('Unspecified Error', target.hostname)

        return self.check(target, stdin, stdout, stderr, exitcode)

    def check(self, target, stdin, stdout, stderr, exitcode):
        """stub. needs to be overwritten by inherited classes"""

        return


class ZypperDowngrade(Downgrade):

    def __init__(self, targets, packages, patches):
        Downgrade.__init__(self, targets, packages)

        self.list_command = 'zypper se -s --match-exact -t package %s | grep -v "(System" | grep ^[iv] | sed "s, ,,g" | awk -F "|" \'{ print $2,"=",$4 }\'' % ' '.join(packages)
        self.install_command = 'rpm -q %s &>/dev/null && zypper -n in -C --force-resolution -y -l %s=%s'


class OldZypperDowngrade(Downgrade):

    def __init__(self, targets, packages, patches):
        Downgrade.__init__(self, targets, packages)

        try:
            patch = patches['zypp']
        except KeyError:
            out.critical('required ZYPP patch number for zypper downgrade not found')
            return

        self.list_command = 'zypper se --match-exact -t package %s | grep -v "^[iv] |[[:space:]]\+|" | grep ^[iv] | sed "s, ,,g" | awk -F "|" \'{ print $4,"=",$5 }\'' % ' '.join(packages)
        self.install_command = 'rpm -q %s &>/dev/null && (line=$(zypper se --match-exact -t package %s | grep %s); repo=$(zypper sl | grep "$(echo $line | cut -d \| -f 2)" | cut -d \| -f 6); if expr match "$repo" ".*/DVD1.*" &>/dev/null; then subdir="suse"; else subdir="rpm"; fi; url=$(echo -n "$repo/$subdir" | sed -e "s, ,,g" ; echo $line | awk \'{ print "/"$11"/"$7"-"$9"."$11".rpm" }\'); package=$(basename $url); if [ ! -z "$repo" ]; then wget -q $url; rpm -Uhv --nodeps --oldpackage $package; rm $package; fi)'

        commands = []

        invalid_packages = ['glibc', 'rpm', 'zypper', 'readline']
        invalid = set(packages).intersection(invalid_packages)
        if invalid:
            out.critical('crucial package found in package list: %s. please downgrade manually' % list(invalid))
            return

        commands.append('for p in $(zypper patches | grep %s-0 | awk \'BEGIN { FS="|"; } { print $2; }\'); do zypper -n rm -y -t patch $p; done'
                         % patch)

        for package in packages:
            commands.append('zypper -n rm -y -t atom %s' % package)

        self.post_commands = commands

class RedHatDowngrade(Downgrade):

    def __init__(self, targets, packages, patches):
        Downgrade.__init__(self, targets, packages)

        commands = []

        commands.append('yum -y downgrade %s' % ' '.join(packages))

        self.commands = commands


Downgrader = {
    '11': ZypperDowngrade,
    '114': ZypperDowngrade,
    '10': OldZypperDowngrade,
    'YUM': RedHatDowngrade,
}


class Install(object):

    def __init__(self, targets, packages=None):
        self.targets = targets
        self.packages = packages

    def run(self):
        skipped = False

        try:
            for target in self.targets:
                lock = self.targets[target].locked()
                if lock.locked and not lock.own():
                    skipped = True
                    out.warning('host %s is locked since %s by %s. skipping.' % (target, lock.time(), lock.user))
                    if lock.comment:
                        out.info("%s's comment: %s" % (lock.user, lock.comment))
                else:
                    self.targets[target].set_locked()
                    thread = ThreadedMethod(queue)
                    thread.setDaemon(True)
                    thread.start()

            if skipped:
                for target in self.targets:
                    try:
                        self.targets[target].remove_lock()
                    except AssertionError:
                        pass
                raise UpdateError('Hosts locked')

            for command in self.commands:
                RunCommand(self.targets, command).run()

                for target in self.targets:
                    self._check(self.targets[target], self.targets[target].lastin(), self.targets[target].lastout(),
                                self.targets[target].lasterr(), self.targets[target].lastexit())
        except:
            raise
        finally:
            for target in self.targets:
                if not lock.locked:  # wasn't locked earlier by set_host_lock
                    try:
                        self.targets[target].remove_lock()
                    except AssertionError:
                        pass

    def _check(self, target, stdin, stdout, stderr, exitcode):
        if 'zypper' in stdin and exitcode == 104:
            out.critical('%s: command "%s" failed:\nstdin:\n%s\nstderr:\n%s', target.hostname, stdin, stdout, stderr)
            raise UpdateError(target.hostname, 'package not found')
        if 'A ZYpp transaction is already in progress.' in stderr:
            out.critical('%s: command "%s" failed:\nstdin:\n%s\nstderr:\n%s', target.hostname, stdin, stdout, stderr)
            raise UpdateError(target.hostname, 'update stack locked')
        if 'System management is locked' in stderr:
            out.critical('%s: command "%s" failed:\nstdin:\n%s\nstderr:\n%s', target.hostname, stdin, stdout, stderr)
            raise UpdateError('update stack locked', target.hostname)
        if 'Error:' in stderr:
            out.critical('%s: command "%s" failed:\nstdin:\n%s\nstderr:\n%s', target.hostname, stdin, stdout, stderr)
            raise UpdateError(target.hostname, 'RPM Error')
        if '(c): c' in stdout:
            out.critical('%s: unresolved dependency problem. please resolve manually:\n%s', target.hostname, stdout)
            raise UpdateError('Dependency Error', target.hostname)

        return self.check(target, stdin, stdout, stderr, exitcode)

    def check(self, target, stdin, stdout, stderr, exitcode):
        """stub. needs to be overwritten by inherited classes"""

        return


class ZypperInstall(Install):

    def __init__(self, targets, packages):
        Install.__init__(self, targets, packages)

        commands = []

        commands.append('zypper -n in -y -l %s' % ' '.join(packages))

        self.commands = commands

class RedHatInstall(Install):

    def __init__(self, targets, packages):
        Install.__init__(self, targets, packages)

        commands = []

        commands.append('yum -y install %s' % ' '.join(packages))

        self.commands = commands


Installer = {
    '11': ZypperInstall,
    '12': ZypperInstall,
    '114': ZypperInstall,
    '10': ZypperInstall,
    'YUM': RedHatInstall,
}


class ZypperUninstall(Install):

    def __init__(self, targets, packages):
        Install.__init__(self, targets, packages)

        commands = []

        commands.append('zypper -n rm %s' % ' '.join(packages))

        self.commands = commands

class RedHatUninstall(Install):

    def __init__(self, targets, packages):
        Install.__init__(self, targets, packages)

        commands = []

        commands.append('yum -y remove %s' % ' '.join(packages))

        self.commands = commands


Uninstaller = {
    '11': ZypperUninstall,
    '12': ZypperUninstall,
    '114': ZypperUninstall,
    '10': ZypperUninstall,
    'YUM': RedHatUninstall,
}
