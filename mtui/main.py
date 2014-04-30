# -*- coding: utf-8 -*-
#
# mtui invocation, usage texts and parameter parsing
#

import os, glob, stat
import sys
import errno
import getopt
import logging
import shutil
import re
from traceback import format_exc
import warnings

from mtui.log import *
from mtui.config import *
from mtui.prompt import *
from mtui.template import TestReport, TestReportFactory
from mtui import __version__

out = logging.getLogger('mtui')

def check_modules():
    """check if all mandatory modules are installed on the system

    currently we need:
    paramiko - for the ssh/network management. this has most likely a
               dependency to python-crypto
    rpm      - for comparing rpm versions and getting rpm metadata
               on the local machine

    """

    modules = {'paramiko': 'python-paramiko', 'rpm': 'rpm-python'}

    for (module, package) in modules.items():
        try:
            with warnings.catch_warnings():
                # paramiko uses some deprecated python code, ignore it since
                # it's only internal stuff and doesn't need to bother the tester
                warnings.filterwarnings('ignore', category=DeprecationWarning)
                exec 'import %s' % module
        except ImportError:
            # exit if a mandatory module couldn't be loaded
            out.error('missing %s module. please install %s' % (module, modules[module]))
            sys.exit(-1)
        else:
            # unload module again after we made sure it exists
            exec 'del %s' % module



def main():
    """parsing parameter list and initializing template metadata"""

    md5 = None

    # default refhosts location. could be an arbitrary string, matches agains
    # the location tags in refhosts.xml
    location = config.location

    # template dir could be as well be set in the environment instead of
    # passing it as command line parameter
    directory = config.template_dir

    # default socket timeout in seconds
    timeout = config.connection_timeout
    attributes = ''

    # default mtui mode is interactive with the QA> shell
    interactive = True

    # default hosts state is enabled instead of disabled or dry-run
    state = 'enabled'

    # overwrites the hostnames set in the template
    refhosts = {}

    # run commands from file before running the cmdloop
    prerun = []

    targets = {}

    try:
        # leave parameter parsing to the getopt module. to add a new parameter
        # extend the short parameter list by a character (and a colon if the
        # parameter takes an argument) and the long parameter list by the
        # full name of the parameter (and an equal sign if the parameter takes
        # an argument. for further information read the python getopt manual.
        short_opts = 'nhdl:s:p:t:m:vw:o:V'
        long_opts  = [
            'non-interactive',
            'help',
            'dryrun',
            'location=',
            'templates=',
            'md5=',
            'search-hosts=',
            'prerun=',
            'overwrite=',
            'verbose',
            'timeout',
            'version',
        ]
        opts, args = getopt.getopt(sys.argv[1:], short_opts, long_opts)
    except getopt.GetoptError, error:
        # catch unkown parameters and show usage
        out.error('failed to parse parameter: %s' % str(error))
        usage()

    for (parameter, argument) in opts:
        if parameter in ('-h', '--help'):
            usage()
        elif parameter in ('-V', '--version'):
            print __version__
            sys.exit(0)
        elif parameter in ('-m', '--md5'):
            # match for a alphanumeric string with a length of 32 chars.
            # if it looks like a duck, swims like a duck, and quacks like
            # a duck, then it probably is a md5 hash.
            match = re.match(r'([a-fA-F\d]{32})', argument)
            try:
                md5 = match.group(1)
            except AttributeError:
                pass
        elif parameter in ('-d', '--dryrun'):
            state = 'dryrun'
        elif parameter in ('-l', '--location'):
            location = argument
        elif parameter in ('-t', '--templates'):
            directory = argument
        elif parameter in ('-n', '--non-interactive'):
            interactive = False
        elif parameter in ('-s', '--search-hosts'):
            # just in case someone specified a comma separated list instead
            # of space separated
            attributes = argument.replace(',', ' ')
        elif parameter in ('-o', '--overwrite'):
            for value in argument.replace(';', ' ').split():
                hostname, _, system = value.partition(',')
                refhosts[hostname] = system
            if not refhosts:
                out.error('overwrite parameter set without valid host arguments')
                sys.exit(0)
        elif parameter in ('-p', '--prerun'):
            try:
                with open(argument, 'r') as script:
                    prerun = script.readlines()
            except Exception:
                out.error('failed to open prerun script')
                sys.exit(0)
        elif parameter in ('-v', '--verbose'):
            out.setLevel(level=logging.DEBUG)
        elif parameter in ('-w', '--timeout'):
            try:
                timeout = int(argument)
            except Exception:
                out.error('wrong timeout value')
                sys.exit(0)
        else:
            usage()

    tr = TestReportFactory(config, out, md5)
    if refhosts:
        tr.systems = refhosts
    else:
        try:
            tr.load_systems_from_testplatforms()
        except Exception as e:
            out.error(format_exc())

    targets = tr.connect_targets()

    prompt = CommandPrompt(targets, tr, config, out)
    prompt.interactive = interactive

    for line in prerun:
        if line.startswith('#'):
            continue
        line = line.rstrip()
        method, _, args = line.partition(' ')
        print 'QA > %s' % line
        try:
            getattr(prompt, 'do_%s' % method)(args)
        except KeyboardInterrupt:
            # stop non-interactive command execution on CTRL-C
            interactive = True
            prompt.interactive = interactive
            break

    while True:
        try:
            if interactive:
                # start the command prompt loop. this call blocks until the
                # end of the QA> session
                prompt.cmdloop()
            else:
                # if we are not in interactive mode, apply the update, export
                # logs to the template and exit saving the template.
                prompt.do_update('all')
                prompt.do_export(None)
                prompt.do_quit(None)
        except KeyboardInterrupt:
            print


def usage():
    """print a simple usage output and exit

    please keep it up to date if new parameters were added

    """

    print
    print 'Maintenance Test Update Installer'
    print '=' * 35
    print
    print sys.argv[0], '[parameter]'


    opts_desc = [
        ('l' ,  'location='       ,  'reference host location name'),
        ('t' ,  'template='       ,  'template directory'),
        ('m' ,  'md5='            ,  'md5 update identifier'),
        ('n' ,  'non-interactive' ,  'non-interactive update shell'),
        ('d' ,  'dryrun'          ,  'start in dryrun mode'),
        ('s' ,  'search-hosts='   ,  'search for hosts matching comma separated tags'),
        ('o' ,  'overwrite='      ,  'overwrite template hostlist ("hostname,system hostname,system")'),
        ('p' ,  'prerun='         ,  'script with a set of MTUI commands to run at start'),
        ('v' ,  'verbose'         ,  'enable debugging output'),
        ('w' ,  'timeout'         ,  'execution timeout in seconds'),
        ('V' ,  'version'         ,  'print version'),
    ]

    print
    print 'parameters:'
    for x in opts_desc:
        kw = dict(zip(('short', 'long', 'description'), x))
        print '\t-{short},--{long:20}{description}'.format(**kw)
    print

    sys.exit(0)


