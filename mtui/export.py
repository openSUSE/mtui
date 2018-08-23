# -*- coding: utf-8 -*-
#
# exporting mtui's log (xml) to the maintenance template
#

import os
import codecs
import xml.dom.minidom
from logging import getLogger
import re

from qamlib.types.rpmver import RPMVersion
from mtui.systemcheck import system_info

logger = getLogger("mtui.export")


def _read_xmldata(xmldata):
    try:
        if os.path.isfile(xmldata):
            x = xml.dom.minidom.parse(xmldata)
        else:
            x = xml.dom.minidom.parseString(xmldata.encode('utf-8'))
    except Exception as error:
        logger.error('failed to parse XML data: {!s}'.format(error))
        raise AttributeError('XML')

    return x


def xml_installog_to_template(xmldata, config, target):
    x = _read_xmldata(xmldata)
    t = []
    for host in x.getElementsByTagName('host'):
        if host.getAttribute('hostname') == target:
            template_log = host.getElementsByTagName('log')[0]

    # add hostname to indicate from which host the log was exported
    updatehost = template_log.parentNode.getAttribute('hostname')

    t.append("log from {!s}\n".format(updatehost))

    for child in template_log.childNodes:
        if not hasattr(child, 'getAttribute'):
            continue
        cmd = child.getAttribute('name')
        if not cmd.startswith('zypper ') and not cmd.startswith('transactional-update'):
            continue
        t.append(
            '# {!s}\n{!s}\n'.format(cmd, child.childNodes[0].nodeValue))
    return t


def fill_template(review_id, template, xmldata, config, smelt):
    if not smelt:
        logger.warning("No data from SMELT api")
        return _xml_to_template(review_id, template, xmldata, config, smelt_output=None, openqa_links=None)

    logger.debug("parse smelt data and prepare pretty report")
    openqa_links = smelt.openqa_links_verbose()
    if openqa_links:
        openqa_links = ["openQA tests:\n", "=============\n",
                        "\n"] + [a + '\n' for a in openqa_links] + ['\n']

    smelt_output = smelt.pretty_output()
    if smelt_output:
        smelt_output = [
            "SMELT Checkers:\n", "===============\n"] + smelt_output

    return _xml_to_template(review_id, template, xmldata, config, smelt_output, openqa_links)


def cut_smelt_data(template, config):
    # returns None if Smelt checkers shorter than 10 lines
    # returns tuple ( template , checkers ) .. smelt has more than 10 lines
    # TODO make it confiruable
    threshold = 10

    try:
        start = template.index("SMELT Checkers:\n")
    except ValueError:
        logger.debug("No smelt data in template")
        return template, None

    end = template.index('REGRESSION TEST SUMMARY:\n', start)

    if end - start < threshold:
        return template, None
    else:
        smelt = template[start:end]
        del template[start+threshold:end]

        template.insert(start+threshold, "\n")
        template.insert(start+threshold, "Rest of SMELT checkers results were moved to checkers.log file, please check it\n")
        template.insert(start+threshold, "\n")
        logger.info("Checkers results were stripped and moved to checkers.log file")
    return template, smelt


def _xml_to_template(review_id, template, xmldata, config, smelt_output, openqa_links):
    """ export mtui xml data to an existing maintenance template

    simple method to export package versions and
    update log from the log to the template file

    Keyword arguments:
    review_id -- mtui.types.obs.RequestReviewID
    template  -- maintenance template path (needs to exist)
    xmldata   -- mtui xml log
    """
    x = _read_xmldata(xmldata)
    current_host = None
    hosts = [h.getAttribute('hostname') for h in x.getElementsByTagName('host')]

    with codecs.open(template, 'r', 'utf-8', errors='replace') as f:
        lines = f.readlines()
        t = []
        # We want to avoid the repeating the scripts outcome, so we delete them
        # from the current template if they are present in the new xmldata
        for line in lines:
            match = re.search(r"reference host:\s (.*)$", line)
            if match:
                current_host = match.group(0)
                continue
            if not re.search(r"\s:\s(SUCCEEDED|(?<!PASSED/)FAILED|INTERNAL ERROR)", line) or current_host not in hosts:
                t.append(line)

    # since the maintenance template is more of a human readable file then
    # a pretty parsable log, we need to build on specific strings to know
    # where to add which information. if these strings change, we need to
    # adapt.

    # for each host/system of the mtui session, search for the correct location
    # in the template. disabled hosts are not excluded.
    # if the location was found, add the hostname.
    # if the location isn't found, it's considered that it doesn't exist.
    # in this case, a whole new host section including the systemname is added.
    for host in x.getElementsByTagName('host'):
        hostname = host.getAttribute('hostname')
        systemtype = host.getAttribute('system')
        # systemname/reference host string in the maintenance template
        # in case the hostname is already set
        line = '{!s} (reference host: {!s})\n'.format(systemtype, hostname)
        try:
            # position of the system line
            i = t.index(line)
        except ValueError:
            # system line not found
            logger.debug(
                'host section %s not found, searching system'.format(hostname))
            # systemname/reference host string in the maintenance template
            # in case the hostname is not yet set
            line = '{!s} (reference host: ?)\n'.format(systemtype)
            try:
                # trying again with a not yet set hostname
                i = t.index(line)
                t[i] = '{!s} (reference host: {!s})\n'.format(
                    systemtype, hostname)
            except ValueError:
                # system line still not found (not with already set hostname, nor
                # with not yet set hostname). create new one
                logger.debug(
                    'system section {!s} not found, creating new one'.format(
                        systemtype))
                # starting point, just above the hosts section
                line = 'Test results by product-arch:\n'

                try:
                    i = t.index(line) + 2
                except ValueError:
                    # starting point not found, try again with the deprecated one
                    # from older templates
                    try:
                        line = 'Test results by test platform:\n'
                        i = t.index(line) + 2
                    except ValueError:
                        # no hostsection found and no starting point for insertion,
                        # bail out and try the next host
                        logger.error('update results section not found')
                        break

                # insert new package version log at position i.
                # example:
                # sles11sp1-i386 (reference host: leo.suse.de)
                # --------------
                # before:
                # after:
                # scripts:
                #
                # => PASSED/FAILED
                #
                # comment: (none)
                #
                t.insert(i, '\n')
                i += 1
                t.insert(
                    i, '{!s} (reference host: {!s})\n'.format(
                        systemtype, hostname))
                i += 1
                t.insert(i, '--------------\n')
                i += 1
                if systemtype.startswith('caasp'):
                    t.insert(
                        i, 'Please check the install logs for the transactional update on host {!s}\n\n'.format(hostname))
                    continue

                t.insert(i, 'before:\n')
                i += 1
                t.insert(i, 'after:\n')
                i += 1
                t.insert(i, 'scripts:\n')
                i += 1
                t.insert(i, '\n')
                i += 1
                t.insert(i, '=> PASSED/FAILED\n')
                i += 1
                t.insert(i, '\n')
                i += 1
                t.insert(i, 'comment: (none)\n')
                i += 1
                t.insert(i, '\n')

    # add package version log and script results for each host to the template
    for host in x.getElementsByTagName('host'):
        versions = {}
        hostname = host.getAttribute('hostname')
        systemtype = host.getAttribute('system')

        # Skip the caasp hosts
        if systemtype.startswith('caasp'):
            continue

        # search for system position which is already existing in the template
        # or was created in the previous step.
        line = '{!s} (reference host: {!s})\n'.format(systemtype, hostname)
        try:
            i = t.index(line)
        except ValueError:
            # host section not found (this should really not happen)
            # proceed with the next one.
            logger.warning('host section {!s} not found'.format(hostname))
            continue
        for state in ['before', 'after']:
            versions[state] = {}
            try:
                i = t.index('      {!s}:\n'.format(state), i) + 1
            except ValueError:
                try:
                    i = t.index('{!s}:\n'.format(state), i) + 1
                except ValueError:
                    logger.error(
                        '{!s} packages section not found'.format(state))
                    continue

            for package in host.getElementsByTagName(state):
                for child in package.childNodes:
                    try:
                        name = child.getAttribute('name')
                        version = child.getAttribute('version')
                        versions[state].update({name: version})

                        # if the package version was already exported, overwrite it with
                        # the new version. if the package version was not yet exported,
                        # add a new line
                        if name in t[i]:
                            # if package version is 0, package isn't installed
                            if version != 'None':
                                t[i] = '\t{!s}-{!s}\n'.format(name, version)
                            else:
                                t[i] = '\tpackage {!s} is not installed\n'.format(
                                    name)
                        else:
                            if version != 'None':
                                t.insert(
                                    i, '\t{!s}-{!s}\n'.format(name, version))
                            else:
                                t.insert(
                                    i, '\tpackage {!s} is not installed\n'.format(name))
                        i += 1
                    except Exception:
                        pass
        try:
            # search for scripts starting point
            i = t.index('scripts:\n', i - 1) + 1
        except ValueError:
            # if no scripts section is found, add a new one
            logger.debug('scripts section not found, adding one')
            t.insert(i, '      scripts:\n')
            i += 1

        template_log = host.getElementsByTagName('log')[0]

        # if the package versions were not updated or one of the testscripts
        # failed, set the result to FAILED, otherwise to PASSED
        failed = 0
        for package in list(versions['before'].keys()):
            # check if the packages have a higher version after the update
            try:
                if versions['after'][package] != 'None' and versions['before'][package] != 'None':
                    assert(
                        RPMVersion(
                            versions['before'][package]) < RPMVersion(
                            versions['after'][package]))
            except Exception:
                failed = 1
        if failed == 1:
            logger.warning(
                'installation test result on {!s} set to FAILED as some packages were not updated. please override manually.'.format(hostname))

        # temporary variable to avoid repeating the same script. We only want the
        # last result, so we store the previous position
        scripts = {}
        for child in template_log.childNodes:
            # search for check scripts in the xml and inspect return code
            # return code values:   0 SUCCEEDED
            #                       1 FAILED
            #                       2 INTERNAL ERROR
            #                       3 NOT RUN
            try:
                # name == command, exitcode == exitcode
                name = child.getAttribute('name')
                exitcode = child.getAttribute('return')
                # move on if the script wasn't run
                if exitcode == '3':
                    continue

            except Exception:
                continue

                # check if command is a compare_* script
            if 'scripts/compare/compare_' in name:
                scriptname = os.path.basename(name.split(' ')[0])
                scriptname = scriptname.replace('compare_', '')
                scriptname = scriptname.replace('.pl', '')
                scriptname = scriptname.replace('.sh', '')

                if exitcode == '0':
                    result = 'SUCCEEDED'
                elif exitcode == '1':
                    failed = 1
                    result = 'FAILED'
                else:
                    failed = 1
                    result = 'INTERNAL ERROR'

                scriptline = '\t{0:25}: {1}\n'.format(scriptname, result)

                if scriptname in scripts:
                    t[scripts[scriptname]] = scriptline
                else:
                    scripts[scriptname] = i
                    if scriptname in t[i]:
                        t[i] = scriptline
                    else:
                        t.insert(i, scriptline)

                    i += 1

        if 'PASSED/FAILED' in t[i + 1]:
            if failed == 0:
                t[i + 1] = '=> PASSED\n'
            elif failed == 1:
                t[i + 1] = '=> FAILED\n'

    # Add output of checkers and link to openQA
    i = t.index('REGRESSION TEST SUMMARY:\n', 0)

    if smelt_output and "SMELT Checkers:\n" not in t:
        t.insert(i, '\n')
        for line in reversed(smelt_output):
            t.insert(i, line)

    if openqa_links and "openQA tests:\n" not in t:
        for line in reversed(openqa_links):
            t.insert(i, line)

    i = t.index('zypper update log:\n', 0) + 1
    add_empty_line = 0
    for host in x.getElementsByTagName('host'):
        hostname = host.getAttribute('hostname')
        install_log = '{!s}/{!s}/{!s}/{!s}.log\n'.format(
            config.reports_url, review_id, config.install_logs, hostname)
        if install_log not in t[i:]:
            i += 1
            t.insert(i, install_log)
            add_empty_line = 1
    if add_empty_line:
        t.insert(i + 1, '\n')

    system_information = system_info(
        config.distro, config.distro_ver, config.distro_kernel, config.session_user)
    # Avoid adding the same info everytime we export
    if system_information != t[-1]:
        t.append(system_information)

    # Remove any possible duplicated lines
    previous_line = None
    lines = []
    for current_line in t:
        if previous_line is None:
            lines.append(current_line)
        elif previous_line == current_line and current_line not in ['\n']:
            None
        else:
            lines.append(current_line)

        previous_line = current_line

    return lines
