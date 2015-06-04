.. vim: set tw=72 sts=2 sw=2 et

########################################################################
                                  FAQ
########################################################################

.. contents:: Table of Contents
    :depth: 2

Running MTUI
############

Non-interactive mode
====================

To simply run an update on the testhosts and export the log, it's
sufficient to pass the `-n` or `--non-interactive` option to MTUI.
However, there is an extension to this when also setting a prerun script
with the `-p` (`--prerun`) option. MTUI runs then a user-defined,
non-interactive session.

Example::

 # cat prerun.example
 # this is a prerun example file
 run all,uname -a
 list_hosts

 # mtui -n -p prerun.example -m 3abae703e3caa9c58bc38f6d04a2387b
 info: connecting to merkur.qam.suse.de
 info: connecting to s390t10.suse.de
 info: connecting to dawn.qam.suse.de

 mtui> run all,uname -a
 merkur.qam.suse.de:~> uname -a [0]
 Linux merkur 3.0.31-0.9-default #1 SMP Tue May 22 21:44:30 UTC 2012 (2dc3831) x86_64 x86_64 x86_64 GNU/Linux

 s390t10.suse.de:~> uname -a [0]
 Linux s390t10 3.0.34-0.7-default #1 SMP Tue Jun 19 09:56:30 UTC 2012 (fbfc70c) s390x s390x s390x GNU/Linux

 dawn.qam.suse.de:~> uname -a [0]
 Linux dawn 3.0.31-0.9-pae #1 SMP Tue May 22 21:44:30 UTC 2012 (2dc3831) i686 i686 i386 GNU/Linux

 info: done

 mtui> list_hosts
 dawn.qam.suse.de     (sles11sp2-i386)    : Enabled (parallel)
 s390t10.suse.de      (sles11sp2-s390x)   : Enabled (parallel)
 merkur.qam.suse.de   (sles11sp2-x86_64)  : Enabled (parallel)

 mtui> quit
 save log? (Y/n)
 info: saving output to /suse/ckornacker/testing/testreports//3abae703e3caa9c58bc38f6d04a2387b/output/log.xml
 info: closing connection to merkur.qam.suse.de
 info: closing connection to s390t10.suse.de
 info: closing connection to dawn.qam.suse.de

Change the default refhosts location
====================================

In case you don't want to use the "default" refhost location, i.e. if
you have a local or virtual set of refhosts, you can set your personal
location in the `~/.mtuirc` configuration file.
This eliminates the need to use the `-l`/`--location` option.

Example::

 # cat ~/.mtuirc
 # MTUI user configuration file
 [mtui]
 location=emeavirt

Set the template directory without passing it as command line parameter every time
==================================================================================

The template directory can be set in the MTUI user configuration file
in either `/etc/mtui.cfg` or `~/.mtuirc`. Additionally, setting the
template directory with `$TEMPLATE_DIR` still works but is superseded by
the `template_dir` option in the configuration file.

Example::

 [mtui]
 template_dir = /tmp/testing/testreports/

Change the default editor for the edit command
==============================================

The `$EDITOR` environment variable is used to get the editor and
defaults to ``vi``.  For the `commit` command, svn checks for the
`$SVN_EDITOR` variable.

Example::

 # EDITOR=nano mtui -m 079ad960654954493d434c03dc3c5543

Distinguish among different MTUI sessions
=========================================

From time to time it's feasible to have multiple MTUI sessions with
different updates active. Usability might suffer in this case since
there is no easy way to distinguish different sessions at first glance.
With the `set_session_name` command, each MTUI session can be named as
part of the prompt string.

Example::

 mtui>

 mtui> set_session_name sle10-bind

 QA:sle10-bind >

Run MTUI without loading a testreport
=====================================

Loading a testreport at start isn't mandatory. After startup, a bare
shell is able to run remote commands on all connected hosts.
Since no hosts are loaded at start, adding hosts to the session could
either be done with the `-s` option or the corresponding MTUI commands.

Example::

 # mtui
 mtui> list_metadata
 error: TestReport not loaded

 mtui> load_template e65aa2ec57d9176e397c320d3d69a370
 info: connecting to merkur.qam.suse.de
 info: connecting to s390vsw116.suse.de
 info: connecting to s390vsw020.suse.de
 info: connecting to dawn.qam.suse.de

 mtui> list_metadata
 MD5SUM         : e65aa2ec57d9176e397c320d3d69a370
 SWAMP ID       : 49275
 Category       : security
 Reviewer       :
 Packager       : mrueckert@suse.com
 SAT            : 6833
 Bugs           : 775649, 775653
 Hosts          : dawn.qam.suse.de merkur.qam.suse.de s390vsw020.suse.de s390vsw116.suse.de
 Packages       : hawk hawk-templates
 Build          : http://hilbert.nue.suse.com/abuildstat/patchinfo/e65aa2ec57d9176e397c320d3d69a370/
 Testreport     : http://qam.suse.de/testreports/e65aa2ec57d9176e397c320d3d69a370/log

Connect to a remote login shell within MTUI
===========================================

The `shell` command invokes a remote login shell on the target host.

Example::

 mtui> shell merkur.qam.suse.de
 Last login: Fri Nov  9 16:40:28 2012 from f167.suse.de
 --------------------------------------------------------------------
 M A I N T E N A N C E    U P D A T E    R E F E R E N C E    H O S T
 * * * * *    O n l y   a u t h o r i z e d   s t a f f   * * * * * *
 --------------------------------------------------------------------

 This is the reference host for

 Product:      SLES 11 SP2
 Architecture: x86_64

 Don't change anything on this system, if you're not allowed to do so.

 Make sure you are familiar with
 https://wiki.innerweb.novell.com/index.php/RD-OPS_QA/HowTos/reference_host_setup
 ---------------------------------------------------------------------

 merkur:~ #

Spawn my favorite terminal emulator on all refhosts
===================================================

MTUI offers an interface for the tester to add his own script to spawn
a terminal emulator on the refhosts. MTUI passes the hostnames to the
script and the script should connect a shell to that hosts.
Currently, scripts for `gnome-terminal` (GNOME), `konsole` (KDE)
and `xterm` are available.

Example::

 mtui> terms gnome

Testreport
##########

Edit the testreport within MTUI
===============================

The `edit` command offers several handy parameters to edit update related
files.

Example::

 mtui> edit template

Get the URL to the testreport
=============================

If the current testreport was already commited to the central
repository, the `list_metadata` command lists the testreport URL,
among other things.

Example::

 mtui> list_metadata
 MD5SUM         : 079ad960654954493d434c03dc3c5543
 SWAMP ID       : 47727
 Category       : recommended
 Reviewer       :
 Packager       : ptesarik@suse.com
 SAT            : 6410
 Bugs           : 718684, 765175
 Hosts          : dawn.qam.suse.de merkur.qam.suse.de
 Packages       : kdump
 Build          : http://hilbert.nue.suse.com/abuildstat/patchinfo/079ad960654954493d434c03dc3c5543/
 Testreport     : http://qam.suse.de/testreports/079ad960654954493d434c03dc3c5543/log

Export the update log from a specific refhost
=============================================

MTUI exports the update log from the first refhost into the testreport
by default.  Simply add the hostname as second parameter to the `export`
command.

Example::

 mtui> export dawn.qam.suse.de
 warning: file /suse/ckornacker/testing/testreports//079ad960654954493d434c03dc3c5543/log exists.
 should i overwrite /suse/ckornacker/testing/testreports//079ad960654954493d434c03dc3c5543/log? (y/N) y
 info: exporting XML to /suse/ckornacker/testing/testreports//079ad960654954493d434c03dc3c5543/log
 wrote template to /suse/ckornacker/testing/testreports//079ad960654954493d434c03dc3c5543/log

Avoid exporting the check script results of a specific host into the testreport
===============================================================================

MTUI exports the results of all hosts from the list to the testreport,
even the disabled ones. This means that all hosts which are for example
temporarily added to the session need to be removed in order to not add them
to the testreport.

Example::

 mtui> remove_host merkur.qam.suse.de
 info: closing connection to merkur.qam.suse.de

Packages
########

Install new packages introduced by the update
=============================================

In case the update introduces new packages which are only available in the
TESTING repositories (which is the case on almost every feature update),
the packages aren't installed by prepare since they are not yet available.
However, the `newpackage` flag of the `update` command installs all
packages right before the post-check scripts are run.
With the `newpackage` flag applied, the tester doesn't need to install
these packages manually.

Example::

 mtui> update all,newpackage
 info: preparing
 info: done
 info: preparing script check_vendor_and_disturl.pl
 info: preparing script check_dependencies.sh
 info: preparing script check_new_licenses.sh
 info: updating
 info: preparing
 info: done
 info: preparing script check_vendor_and_disturl.pl
 info: preparing script check_dependencies.sh
 info: preparing script check_new_licenses.sh
 info: preparing script compare_vendor_and_disturl.pl
 info: preparing script compare_dependencies.sh
 info: preparing script compare_new_licenses.sh
 info: done

Switch from TESTING repositories to UPDATE and vice versa
=========================================================

The `set_repo` command uses `rep-clean.sh` to switch between `TESTING`
and `UPDATE` repos on the refhosts. The repositories need to be
named according to `rep-clean.sh` conventions for this to work.

Example::

 mtui> set_repo all,testing

Force installation of a conflicting package
===========================================

Package installation can be forced either with the `prepare` command and
the `force` parameter or the `update` command and the `force` parameter.

Example::

 mtui> prepare all,force
 info: preparing
 info: done

Avoid installing packages which are listed in the patchinfo but are not installable
===================================================================================

To avoid installing additional packages, add the `installed` parameter
either to the `prepare` or the `update` command.

Example::

 mtui> update all,installed
 info: preparing
 info: done
 info: updating
 info: done

Update the tested packages without running the check scripts
============================================================

The `prepare` command installs all packages from the `TESTING`
repositories if the `testing` parameter is set.

Example::

 mtui> prepare dawn.qam.suse.de,testing
 info: preparing
 info: done

Install unrelated package to the refhosts
=========================================

MTUI manages install and uninstall operations with the respective commands.
The repositories are not changed during the installation.

Example::

 mtui> install all,gnome-js-common
 info: installing
 info: done

 mtui> uninstall all,gnome-js-common
 info: removing
 info: done

Get a diff between the old packages and the new testing packages
================================================================

Make sure that the old package versions are installed on the refhosts.
Run the `source_diff source` command to get a diff of the source rpm
(from build service) content or run `source_diff binary` to get an
architecture-dependent diff with all patches applied to the sources.
In case of already updated refhosts, use the `downgrade` command to get
the accurate packages first.
With the `edit` command and the printed diff path, it's easy to open
the diff within MTUI.

Example::

 mtui> source_diff source
 348 blocks
 info: src rpm was extracted to /tmp/079ad960654954493d434c03dc3c5543
 info: wrote diff locally to /tmp/079ad960654954493d434c03dc3c5543/kdump-source.diff

 mtui> edit file,/tmp/079ad960654954493d434c03dc3c5543/kdump-source.diff

Make sure that all patches mentioned in the specfile are applied
================================================================

The `source_verify` command gives a hint if the mentioned patches are
applied.  To be completely sure, a manual check is recommended.

Example::

 mtui> source_verify
 Patches in /tmp/03fe7308f8c5df7a1dcf4f9036872052/udev/udev.spec:
 0005-cdrom_id-retry-to-open-the-device-if-EBUSY.patch: applied
 0334-keymap-Fix-invalid-map-line.patch       : applied
 0335-keymap-include-linux-limits.h.patch     : applied
 0336-keymap-linux-input.h-get-absolute-include-path-from-.patch: applied


Refhosts
########

List refhosts matching a specific criteria
==========================================

The `search_hosts` command offers a search interface to refhosts
database.  The command knows several keywords concerning host features.

Example (search for all SLES 11 SP2 hosts which have webyast installed)::

 mtui> search_hosts sles 11 sp2 webyast
 merkur.qam.suse.de       : sles 11sp2 x86_64 hae webyast 1.2 sdk
 s390t10.suse.de          : sles 11sp2 s390x hae webyast 1.2 sdk
 sinope.qam.suse.de       : sles 11sp2 ia64 hae webyast 1.2 sdk
 aal.qam.suse.de          : sles 11sp2 ppc64 hae webyast 1.2 sdk
 dawn.qam.suse.de         : sles 11sp2 i386 hae webyast 1.2 sdk

Add a refhost to the list
=========================

The `autoadd` command can add a specific host to the list if a existing
hostname (in refhosts.xml) was set, or a list of hosts if attributes were
supplied.

Example::

 mtui> autoadd merkur.qam.suse.de
 merkur.qam.suse.de       : sles 11sp2 x86_64 hae webyast 1.2 sdk
 info: connecting to merkur.qam.suse.de

 mtui> autoadd i386 x86_64 sles 11 sp2 webyast
 merkur.qam.suse.de       : sles 11sp2 x86_64 hae webyast 1.2 sdk
 dawn.qam.suse.de         : sles 11sp2 i386 hae webyast 1.2 sdk
 info: connecting to dawn.qam.suse.de
 info: connecting to merkur.qam.suse.de

Use refhosts from another location
==================================

Changing the current location is possible using the `set_location`
command.

Example::

 mtui> search_hosts sles 11 sp1 xen guest
 xenu32-11-1.qam.suse.de  : sles 11sp1 i386 kernel guest xen sdk
 xenu64-11-1.qam.suse.de  : sles 11sp1 x86_64 kernel guest xen sdk

 mtui> set_location emeavirt
 info: changed location from "default" to "emeavirt"

 mtui> search_hosts sles 11 sp1 xen guest
 boronen.qam.suse.de      : sles 11sp1 x86_64 guest xen sdk
 teladi.qam.suse.de       : sles 11sp1 i386 guest xen sdk

Override refhosts mentioned in the testreport
=============================================

Usually it's sufficient to simply load the hosts from the testreport and
add or remove refhosts with the appropriate commands.
For some corner-cases, like exclusive automated testing on virtual
machines, the hostlist could be overwritten with the `-o` option.

Example::

 # mtui -s d51.suse.de,sle11test -s d122.suse.de,sp2test \
   -m 079ad960654954493d434c03dc3c5543
 info: connecting to d51.suse.de
 info: connecting to d122.suse.de
 mtui>

Run commands only on a subset of the hostlist
=============================================

From time to time it may be useful to `update`, `downgrade` or `run`
a command only on a subset of refhosts while staying connected to the
others.
The `set_host_state` command temporary disables and/or enables specific
hosts.

Example::

 mtui> list_hosts
 dawn.qam.suse.de     (sles11sp2-i386)    : Enabled (parallel)
 sinope.qam.suse.de   (sles11sp2-ia64)    : Enabled (parallel)
 aal.qam.suse.de      (sles11sp2-ppc64)   : Enabled (parallel)
 s390t10.suse.de      (sles11sp2-s390x)   : Enabled (parallel)
 merkur.qam.suse.de   (sles11sp2-x86_64)  : Enabled (parallel)

 mtui> set_host_state sinope.qam.suse.de,dawn.qam.suse.de,merkur.qam.suse.de,disabled

 mtui> list_hosts
 dawn.qam.suse.de     (sles11sp2-i386)    : Disabled (parallel)
 sinope.qam.suse.de   (sles11sp2-ia64)    : Disabled (parallel)
 aal.qam.suse.de      (sles11sp2-ppc64)   : Enabled (parallel)
 s390t10.suse.de      (sles11sp2-s390x)   : Enabled (parallel)
 merkur.qam.suse.de   (sles11sp2-x86_64)  : Disabled (parallel)

 mtui> set_host_state all,enabled

 mtui> list_hosts
 dawn.qam.suse.de     (sles11sp2-i386)    : Enabled (parallel)
 sinope.qam.suse.de   (sles11sp2-ia64)    : Enabled (parallel)
 aal.qam.suse.de      (sles11sp2-ppc64)   : Enabled (parallel)
 s390t10.suse.de      (sles11sp2-s390x)   : Enabled (parallel)
 merkur.qam.suse.de   (sles11sp2-x86_64)  : Enabled (parallel)

Remove dangling MTUI locks after crash
======================================

Run MTUI on the same hosts again and remove the locks using
the `run` command.

Example::

 mtui> unlock -f


Tests
#####

Use the Testopia interface of MTUI
==================================

Since there are no generic credentials for using Bugzilla/Testopia, everyone
willing to use Testopia from within MTUI needs to add a valid Bugzilla username
and password to the config file. Currently supported testopia actions are
listing testcases for the current update as well as arbitrary testcases,
editing and creating testcases. Access to the testcases is cached within MTUI
and usually faster than using the Testopia webUI.

Example::

 # cat ~/.mtuirc
 # MTUI user configuration file
 [testopia]
 user=mylogin
 pass=test123

Use a Keyring for securely storing my Testopia password
=======================================================

If `python-keyring` is installed, MTUI supports storing passwords in
GnomeKeyring and KWallet. To store the password, set it in the
configuration file.
To use the password stored in the keyring, remove it from the
configuration file.

Get a list of the Testopia testcases to be run for the update
=============================================================

The `testopia_list` command lists all package testcases for the update.

Example::

 mtui> testopia_list
 try driver update                      : confirmed (manual)
 https://bugzilla.novell.com/tr_show_case.cgi?case_id=232740

 check for rpm errors                   : confirmed (manual)
 https://bugzilla.novell.com/tr_show_case.cgi?case_id=232741

Display the test instructions of a Testopia testcase
====================================================

Testcase actions can be displayed for arbitrary testcase IDs with the
`testopia_show` command.

Example::

 mtui> testopia_show 232740
 Testcase summary: try driver update
 Testcase requirements:
 Testcase URL: https://bugzilla.novell.com/tr_show_case.cgi?case_id=232740
 Testcase actions:

 try to update a driver with a driver CD according to documentation in:
 attachment to bugzilla bug #21040 (http://bugzilla.suse.de/show_bug.cgi?id=21040)

 testcase added after UL-RC4, due to BUG 21040


Run a QA ctcs2 testsuite within MTUI
====================================

The `testsuite_*` commands offer several options to run testsuites
and manage the submission to QADB.

Examples
~~~~~~~~

List available testsuites on a refhost::

 mtui> testsuite_list merkur.qam.suse.de
 testsuites on merkur.qam.suse.de (sles11sp2-x86_64):
 glibc-run
 test_php53-run
 test_postfix-run
 test_samba-run
 test_tiff-run

Run a specific testsuite::

 mtui> testsuite_run dawn.qam.suse.de,test_tiff-run
 dawn.qam.suse.de:~> test_tiff-testsuite [0]
 INFO: Variable TESTS_LOGDIR is set, logs will be stored in /var/log/qa/079ad960654954493d434c03dc3c5543/ctcs2.
 Initializing test run for control file qa_tiff.tcf...
 Current time: Fri Jun 29 18:21:15 CEST 2012
 **** Test in progress ****
 **** Test run complete ****
 Current time: Fri Jun 29 18:22:32 CEST 2012
 Exiting test run..
 Displaying report...
 Total test time: 1m17s
 Tests skipped:
 tiffcrop-R90-logluv-3c-16b.sh ran 1 times in 1s, had skipped on 1 attempts.
 Tests passed:
 bmp2tiff_palette.sh bmp2tiff_rgb.sh common.sh gif2tiff.sh ppm2tiff_pbm.sh

Submit the testsuite results to QADB::

 mtui> testsuite_submit dawn.qam.suse.de,test_tiff-run
 info: please specify rd-qa NIS password
 Password:
 info: submission for dawn.qam.suse.de (sles11sp2-i386): http://qadb.ext.suse.de/qadb/submission.php?submission_id=12495
 info: done


