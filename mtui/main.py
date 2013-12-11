# -*- coding: utf-8 -*-
#
# mtui invocation, usage texts and parameter parsing
#

import os
import sys
import errno
import getopt
import logging
import shutil
import re
import traceback
import warnings

from mtui.log import *
from mtui.config import *
from mtui.prompt import *
from mtui.template import *
from mtui import version

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

def realmain():
    """
    mtui entry point. checking for all needed modules and invoke main()
    """

    try:
        check_modules()

        from mtui.main import main
        main()
    except Exception:
        # all uncatched exceptions end up here and generate a nice backtrace
        out.error('you found a bug. please notify ckornacker@suse.de')
        print 'backtrace:'
        print '-' * 60
        traceback.print_exc(file=sys.stdout)
        print '-' * 60


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
            print version
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

    # make sure that the testreport directory exists
    try:
        os.makedirs(directory)
    except OSError, error:
        if error.errno == errno.EEXIST:
            pass
    except Exception, error:
        out.critical('failed to create testreport directory: %s' % str(error))
        return
    else:
        out.debug('created testreport directory')

    if md5 is not None:
        try:
            update = Template(md5, location, directory)
        except IOError:
            # checkout the current testing template. we could do this with the
            # python svn module, but for now it's simpler calling just system()
            svnpath = '/'.join([config.svn_path, md5])
            os.system('cd %s; svn co %s' % (directory, svnpath))
            try:
                update = Template(md5, location, directory)
            except IOError:
                # in case the template doesn't exist, try to check it out
                out.error('failed to check out testreport template from %s' % svnpath)
                sys.exit(0)
        except Exception:
            out.error('failed to parse testreport template %s' % os.path.join(directory, md5, 'log'))
            sys.exit(0)

        metadata = update.metadata
    else:
        # if metadata isn't filled with data from the template,
        # populate it with the required fields
        metadata = Metadata()
        metadata.location = location
        metadata.directory = directory

    if refhosts:
        metadata.systems = refhosts

    for (host, system) in metadata.systems.items():
        try:
            targets[host] = Target(host, system, metadata.get_package_list(), state=state, timeout=timeout)
            targets[host].add_history(['connect'])
        except Exception:
            out.warning('failed to add host %s to target list' % host)
        except KeyboardInterrupt:
            # skip adding the reference host if CTRL-C was pressed. this might
            # not work if we are somewhere deep in the network/ssh code where
            # KeyboardInterrupt is not thrown.
            out.warning('skipping host %s' % host)

    # ignore svn metadata files when copying the testscripts to
    # the correspondend directories
    ignored = shutil.ignore_patterns('*.svn')

    try:
        assert(metadata.path)
        # copy check_* and compare_* scripts to the template directory
        sourcedir = os.path.join(config.datadir, 'scripts')
        destdir = os.path.join(os.path.dirname(metadata.path), 'scripts')
        shutil.copytree(sourcedir, destdir, ignore=ignored)
    except OSError, error:
        # this should not happen but was already noticed once or twice.
        # probable due to nfs timeouts if mtui was checked out to a nfs mount.
        if error.errno == errno.ENOENT:
            out.warning('scripts/ dir not found, please copy manually')
        else:
            pass
    except AssertionError:
        # metadata path not set. most likely run without template.
        # don't copy anything
        out.debug('running without template, not copying scripts directory')
        pass

    # create QA prompt and add hosts by attributes
    prompt = CommandPrompt(targets, metadata)
    if attributes:
        prompt.do_autoadd(attributes)

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


