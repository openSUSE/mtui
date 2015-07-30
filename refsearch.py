#!/usr/bin/python

from __future__ import print_function

import getopt
import sys
import logging

from mtui.refhost import Attributes
from mtui.prompt import CommandPrompt
from mtui.display import CommandPromptDisplay
from mtui.config import Config

def usage():
    tags = Attributes.tags

    print()
    print('Maintenance Reference Host Search')
    print('=' * 33)
    print()
    print(sys.argv[0], '[parameter] <search>')
    print()
    print('parameters:')
    print('\t-{short},--{long:20}{description}'.format(short='h', long='help', description='help'))
    print('\t-{short},--{long:20}{description}'.format(short='l', long='location=', description='reference host location name'))
    print()
    print('possible search tags:')

    for key in tags:
        print('\t{key:25}{value}'.format(key=key, value=", ".join(tags[key])))
    print()
    print('example:')
    print('\t{name} sles 11 sp3 i386'.format(name=sys.argv[0]))
    print()

def main():
    log = logging.getLogger('mtui')
    config = Config(log)

    try:
        (opts, args) = getopt.getopt(sys.argv[1:], 'hl:', ['help', 'location='])
        assert(opts or args)
        search = args
        for (parameter, argument) in opts:
            if parameter in ('-h', '--help'):
                usage()
            elif parameter in ('-l', '--location'):
                config.location = argument
            else:
                usage()

    except (getopt.GetoptError, AssertionError):
        usage()
        sys.exit(2)

    p = CommandPrompt(config, log, sys, CommandPromptDisplay)
    p.do_search_hosts(" ".join(search))

main()
