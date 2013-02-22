#!/usr/bin/python

import getopt
import sys

from mtui.refhost import *
from mtui.config import *

def do_search_hosts(location, args):
    """
    Seach hosts by by the specified attributes. A attribute tag could also be a
    system type name like sles11sp1-i386 or a hostname.
    search_hosts <attribute> [attribute ...]
    Keyword arguments:
    attribute-- host attributes like architecture or product
    """

    if args:
        attributes = Attributes()

        try:
            refhost = Refhost(config.refhosts_xml, location)
        except Exception:
            print 'failed to load reference hosts data'
            return

        if 'Testplatform:' in args:
            try:
                refhost.set_attributes_from_testplatform(args.replace('Testplatform: ', ''))
                hosts = refhost.search()
            except (ValueError, KeyError):
                hosts = []
                print 'failed to parse Testplatform string'
        elif refhost.get_host_attributes(args):
            hosts = [args]
        else:
            for _tag in args.split(' '):
                tag = _tag.lower()
                match = re.search('(\d+)\.(\d+)', tag)
                if match:
                    attributes.major = match.group(1)
                    attributes.minor = match.group(2)
                if tag in attributes.tags['products']:
                    attributes.product = tag
                if tag in attributes.tags['archs']:
                    attributes.archs.append(tag)
                if tag in attributes.tags['addons']:
                    attributes.addons.update({tag:{}})
                if tag in attributes.tags['major']:
                    attributes.major = tag
                if tag in attributes.tags['minor']:
                    attributes.minor = tag
                if tag == 'kernel':
                    attributes.kernel = True
                if tag == 'ltss':
                    attributes.ltss = True
                if tag == '!kernel':
                    attributes.kernel = False
                if tag == '!ltss':
                    attributes.ltss = False
                if tag == 'xenu':
                    attributes.virtual.update({'mode':'guest', 'hypervisor':'xen'})
                if tag == 'xen0':
                    attributes.virtual.update({'mode':'host', 'hypervisor':'xen'})
                if tag == 'xen':
                    attributes.virtual.update({'hypervisor':'xen'})
                if tag == 'kvm':
                    attributes.virtual.update({'hypervisor':'kvm'})
                if tag == 'vmware':
                    attributes.virtual.update({'hypervisor':'vmware'})
                if tag == 'host':
                    attributes.virtual.update({'mode':'host'})
                if tag == 'guest':
                    attributes.virtual.update({'mode':'guest'})

            hosts = refhost.search(attributes)

            # check if some tags were passed to the attributes object which has
            # all archs set by default
            if not set(str(attributes).split()) ^ set(attributes.tags["archs"]):
                return []

        for hostname in set(hosts):
            hosttags = refhost.get_host_attributes(hostname)
            print '{0:25}: {1}'.format(hostname, hosttags)

        return hosts

def usage():
	print
	print 'Maintenance Referenz Host Search'
	print '=' * 33
	print
	print sys.argv[0], '[parameter]'
	print
	print 'parameters:'
	print '\t-{short},--{long:20}{description}'.format(short='h', long='help', description='help')
	print '\t-{short},--{long:20}{description}'.format(short='l', long='location=', description='reference host location name')

def main():
    #parsing parameter and arguments
  
	location = "default"

	try:
		(opts, args) = getopt.getopt(sys.argv[1:], 'hl:', ['help', 'location='])
		search = args	
		for (parameter, argument) in opts:
			if parameter in ('-h', '--help'):
				usage()
			elif parameter in ('-l', '--location'):
				location = location
			else:
				usage()
	
	except getopt.GetoptError, error:
		usage()
		sys.exit(2)

	do_search_hosts(location, " ".join(search))

main()
