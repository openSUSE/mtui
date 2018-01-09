.. vim: set tw=72 sts=2 sw=2 et

########################################################################
                                  FAQ
########################################################################

.. contents:: Table of Contents
    :depth: 3

Running MTUI
############

Can I run MTUI in non-interactive mode?
=======================================

To simply run an update on the testhosts and export the log, it is
sufficient to pass the ``-n`` or ``--non-interactive`` option to MTUI.
However, there is an extension to this when also setting a prerun script
with the ``-p`` (``--prerun``) option. MTUI then runs a user-defined,
non-interactive session.

Example::

  # cat prerun.example
  # this is a prerun example file
  run uname -a
  list_hosts

  # mtui -n -p prerun.example -r SUSE:Maintenance:3601:126030
  info: connecting to edna.qam.suse.cz
  info: connecting to s390vsl048.suse.de
  info: connecting to moe.qam.suse.cz

  mtui> run uname -a
  edna.qam.suse.cz:-> uname -a [0]
  Linux edna 3.12.61-52.66-default #1 SMP Tue Jan 24 12:54:04 UTC 2017 (20041c8/kGraft-98c9d05) x86_64 x86_64 x86_64 GNU/Linux

  s390vsl048.suse.de:-> uname -a [0]
  Linux s390vsl048 3.12.61-52.69-default #1 SMP Thu Mar 23 15:48:05 UTC 2017 (08269c9) s390x s390x s390x GNU/Linux

  moe.qam.suse.cz:-> uname -a [0]
  Linux moe 3.12.61-52.66-default #1 SMP Tue Jan 24 12:54:04 UTC 2017 (20041c8/kGraft-98c9d05) x86_64 x86_64 x86_64 GNU/Linux

  info: done

  mtui> list_hosts
  edna.qam.suse.cz     (sles12_module-x86_64): Enabled (parallel)
  moe.qam.suse.cz      (sles12_module-x86_64): Enabled (parallel)
  s390vsl048.suse.de   (sles12_module-s390x): Enabled (parallel)

  mtui> quit
  save log? (Y/n)
  info: saving output to /suse/testing/testreports/SUSE:Maintenance:3601:126030/output/log.xml
  info: closing connection to edna.qam.suse.cz
  info: closing connection to s390vsl048.suse.de
  info: closing connection to moe.qam.suse.cz

How do I change the default refhosts location?
==============================================

In case you don't want to use the default refhost location, i.e. if
you have a local or virtual set of refhosts, you can set your personal
location in the ``~/.mtuirc`` configuration file.
This eliminates the need to use the ``-l`` (``--location``) option.

Example::

  # cat ~/.mtuirc
  # MTUI user configuration file
  [mtui]
  location=nuremberg

Can I set the template directory without passing it as a command line parameter every time?
===========================================================================================

The template directory can be set in the `MTUI user configuration file`_
in either ``/etc/mtui.cfg`` or ``~/.mtuirc``. Additionally, setting the
template directory with ``$TEMPLATE_DIR`` still works but is superseded by
the ``template_dir`` option in the configuration file.

.. _MTUI user configuration file: http://qam.suse.de/projects/mtui/latest/cfg.html

Example::

  [mtui]
  template_dir = /tmp/testing/testreports/

How can I change the default editor for the edit command?
=========================================================

The ``$EDITOR`` environment variable is used to get the editor and
defaults to ``vi``.  For the ``commit`` command, svn checks for the
``$SVN_EDITOR`` variable.

Example:

.. code-block:: sh

  # EDITOR=nano mtui -r SUSE:Maintenance:3601:126030

Is there a way to easily distinguish among different MTUI sessions?
===================================================================

When you are testing different updates at the same time, you may end up having
multiple active MTUI sessions. Usability might suffer in this case since
there is no easy way to distinguish these different sessions at the first glance.
With the ``set_session_name`` command, you can set a name for each MTUI session.
The name will appear as a part of the prompt string.

Example::

  mtui>

  mtui> set_session_name sle12-bind

  mtui:sle12-bind>

Can I run MTUI without loading a test report?
=============================================

Loading a test report at start isn't mandatory. After the startup, a bare
shell is able to run remote commands on all connected hosts.
Since no hosts are loaded at start, adding hosts to the session can
either be done with the ``-s`` option, or the corresponding MTUI commands.

Example::

  # mtui
  mtui> list_metadata
  error: TestReport not loaded

  mtui> load_template SUSE:Maintenance:3601:126030
  info: connecting to moe.qam.suse.cz
  info: connecting to s390vsl048.suse.de
  info: connecting to edna.qam.suse.cz

  mtui> list_metadata
  Bugs           : 1012780
  Category       : optional
  Hosts          : edna.qam.suse.cz moe.qam.suse.cz s390vsl048.suse.de
  Packager       : lchiquitto@suse.com
  Packages       : nodejs6 nodejs6-devel nodejs6-docs npm6
  Rating         : low
  Repository     : http://download.suse.de/ibs/SUSE:/Maintenance:/3601/
  ReviewRequestID: SUSE:Maintenance:3601:126030
  Reviewer       : snbarth
  Testplatform   : base=sles(major=12,minor=);arch=[s390x,x86_64]
  Testreport     : http://qam.suse.de/testreports/SUSE:Maintenance:3601:126030/log


How can I connect to a remote login shell within MTUI?
======================================================

The ``shell`` command invokes a remote login shell on the target host.

Example::

  mtui> shell -t moe.qam.suse.cz
  Last login: Wed Apr 19 15:22:23 2017 from clumsypotato.suse.cz

  --------------------------------------------------------------------
  M A I N T E N A N C E    U P D A T E    R E F E R E N C E    H O S T
  * * * * *    O n l y   a u t h o r i z e d   s t a f f   * * * * * *
  --------------------------------------------------------------------

  This is the reference host for

       Product = SUSE Linux Enterprise Server 12 LTSS
                 SUSE Linux Enterprise Server 12
                 QA packages for SLE 12
                 SUSE Linux Enterprise Software Bootstrap Kit 12
                 SUSE Linux Enterprise Live Patching
                 Advanced Systems Management Module
                 Containers module
                 Legacy Module
                 Public Cloud Module
                 Toolchain Module
                 Web and Scripting Module
                 SUSE Linux Enterprise Software Development Kit 12
  Architecture = x86_64
      Hostname = moe
        Kernel = 3.12.61-52.66-default
        Uptime = 23 days 5:05 hours
           CPU = QEMU Virtual CPU version 2.0.0
           RAM = 1663680 kB / 1791272 kb
   Temperature = No sensors found!

       Comment =

        ssh connected user from:
  clumsypotato.suse.cz:48258 (ESTABLISHED)

  Don't change anything on this system, if you're not allowed to
  do so.

  Make sure you are familiar with:
  https://pes.suse.de/QA_Maintenance/
  ---------------------------------------------------------------------

  moe:~ #


Can I spawn my favorite terminal emulator on all refhosts?
==========================================================

MTUI offers an interface for the tester to add his own script to spawn
a terminal emulator on the refhosts. MTUI passes the hostnames to the
script and the script should connect a shell to t hosts.
Currently, scripts for gnome-terminal (``gnome``), konsole (``kde``), ``xterm``,
``tmux``,  ``urxvtc``, ``sakura`` and ``screen`` are available.

Example::

  mtui> terms gnome

Test report
###########

How can I edit the test report within MTUI?
===========================================

Just use the ``edit`` command with no parameters. If you want to edit a different
update-related file, add its name as a parameter to the ``edit`` command.

Example::

  mtui> edit

How do I get the URL to the test report?
========================================

If the current test report was already commited to the central
repository, the ``list_metadata`` command lists the test report URL,
among other things.

Example::

  mtui> list_metadata
  Bugs           : 1012780
  Category       : optional
  Hosts          : edna.qam.suse.cz moe.qam.suse.cz s390vsl048.suse.de
  Packager       : lchiquitto@suse.com
  Packages       : nodejs6 nodejs6-devel nodejs6-docs npm6
  Rating         : low
  Repository     : http://download.suse.de/ibs/SUSE:/Maintenance:/3601/
  ReviewRequestID: SUSE:Maintenance:3601:126030
  Reviewer       : snbarth
  Testplatform   : base=sles(major=12,minor=);arch=[s390x,x86_64]
  Testreport     : http://qam.suse.de/testreports/SUSE:Maintenance:3601:126030/log


Can I export the update log from a specific refhost?
====================================================

MTUI exports the update log from the first refhost into the test report
by default. In case you want to export the log from a specific refhost, you can
do so by using the ``-it`` parameter and adding the hostname to the ``export``
command.

Example::

  mtui> export -t edna.qam.suse.de
  warning: file /suse/testing/testreports//SUSE:Maintenance:3601:126030/log exists.
  should i overwrite /suse/testing/testreports//SUSE:Maintenance:3601:126030/log? (y/N) y
  info: exporting XML to /suse/testing/testreports//SUSE:Maintenance:3601:126030/log
  wrote template to /suse/testing/testreports//SUSE:Maintenance:3601:126030/log

How do I avoid exporting the check script results of a specific host into the test report?
==========================================================================================

MTUI exports the results of all hosts from the list to the test report,
even the disabled ones. This means that all hosts which are for example
temporarily added to the session need to be removed in order to not add them
to the test report.

Example::

  mtui> remove_host -t merkur.qam.suse.de
  info: closing connection to merkur.qam.suse.de

Packages
########

How do I install new packages introduced by the update?
=======================================================

In case the update introduces new packages which are only available in the
test update repositories (which is the case on almost every feature update),
the packages cannot be installed by ``prepare`` since they are not yet available.
In such a case, use the ``update`` command with the ``--newpackage`` flag, which
installs all packages right before the post-check scripts are run, saving you
the need to install these packages manually.

Example::

  mtui> update --newpackage
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


Can I force-install a conflicting package?
==========================================

Package installation can be forced by using the ``prepare`` command with
the ``--force`` parameter.

Example::

  mtui> prepare --force
  info: preparing
  [...]
  info: done


Can I avoid installing uninstallable packages which are listed in the patchinfo?
================================================================================

To avoid installing additional packages, add the ``--installed`` parameter to the
``prepare`` command.

Example::

  mtui> prepare --installed
  info: preparing
  [...]
  info: done


Can I update the tested packages without running the check scripts?
===================================================================

The ``prepare`` command installs all packages from the test update
repositories if the ``--update`` parameter is set.

Example::

  mtui> prepare -t edna.qam.suse.cz --update
  info: preparing
  [...]
  info: done

How do I install unrelated package to the refhosts?
===================================================

MTUI manages install and uninstall operations with the respective commands.
The repositories are not changed during the installation.

Example::

  mtui> install gnome-js-common
  info: Installing
  info: Done

  mtui> uninstall gnome-js-common
  info: Removing
  info: Done


Refhosts
########

I need to add a refhost to the list. How can I do it?
=====================================================

The ``add_host`` command adds a specific host to the list. Both hostname and
system type must be provided.

Example::

  mtui> add_host -t craig.qam.suse.cz
  info: connecting to craig.qam.suse.cz


Can I use refhosts from another location?
=========================================

Changing the current location is possible using the ``set_location``
command.

Example::

  mtui> set_location nuremberg
  info: changed location from 'prague' to 'nuremberg'


How can I override refhosts mentioned in the test report?
=========================================================

Usually it's sufficient to simply load the hosts from the test report and
add or remove refhosts with the appropriate commands.
For some corner-cases, like exclusive automated testing on virtual
machines, the host list could be overwritten with the ``-s`` option.

Example::

  # mtui -s edna.qam.suse.cz -s moe.qam.suse.cz -r SUSE:Maintenance:3601:126030
  info: connecting to edna.qam.suse.cz
  info: connecting to moe.qam.suse.cz
  mtui> list_hosts
  edna.qam.suse.cz     (sle12None)             : Enabled (parallel)
  moe.qam.suse.cz      (sle12None)             : Enabled (parallel)
  mtui>

Can I run commands only on a subset of the host list?
=====================================================

From time to time it may be useful to ``update``, ``downgrade`` or ``run``
a command only on a subset of refhosts while staying connected to the
others.
The ``set_host_state`` command temporarily disables and/or enables specific
hosts.

Example::

  mtui> list_hosts
  edna.qam.suse.cz     (sles12_module-x86_64): Enabled (parallel)
  moe.qam.suse.cz      (sles12_module-x86_64): Enabled (parallel)
  s390vsl048.suse.de   (sles12_module-s390x): Enabled (parallel)

  mtui> set_host_state -t edna.qam.suse.cz -t s390vsl048.suse.de disabled
  info: Setting host edna.qam.suse.cz state to disabled
  info: Setting host s390vsl048.suse.de state to disabled

  mtui> list_hosts
  edna.qam.suse.cz     (sles12_module-x86_64): Disabled (parallel)
  moe.qam.suse.cz      (sles12_module-x86_64): Enabled (parallel)
  s390vsl048.suse.de   (sles12_module-s390x): Disabled (parallel)

  mtui> set_host_state enabled
  info: Setting host edna.qam.suse.cz state to enabled
  info: Setting host s390vsl048.suse.de state to enabled
  info: Setting host moe.qam.suse.cz state to enabled

  mtui> list_hosts
  edna.qam.suse.cz     (sles12_module-x86_64): Enabled (parallel)
  moe.qam.suse.cz      (sles12_module-x86_64): Enabled (parallel)
  s390vsl048.suse.de   (sles12_module-s390x): Enabled (parallel)


How do I remove dangling MTUI locks after a crash?
==================================================

Run MTUI on the same hosts again and remove the locks using
the ``unlock`` command.

Example::

  mtui> unlock -f


Tests
#####

How do I use the Testopia interface of MTUI?
============================================

Since there are no generic credentials for using Bugzilla/Testopia, everyone
wanting to use Testopia from within MTUI needs to add a valid Bugzilla username
and password to the config file. Currently supported Testopia actions are
listing test cases for the current update, as well as other, unrelated test cases,
editing and creating test cases. Access to the test cases is cached within MTUI
and usually faster than using the Testopia webUI.

Example::

  # cat ~/.mtuirc
  # MTUI user configuration file
  [testopia]
  user=mylogin
  pass=test123

Can I use a keyring for securely storing my Testopia password?
==============================================================

If ``python-keyring`` is installed, MTUI supports storing passwords in
`GNOME Keyring`_ and `KWallet`_. To store the password, set it in the
configuration file.
To use the password stored in the keyring, remove it from the
configuration file.

.. _GNOME Keyring: https://wiki.gnome.org/Projects/GnomeKeyring
.. _KWallet: https://userbase.kde.org/KDE_Wallet_Manager


How do I get a list of Testopia test cases for my update?
=========================================================

The ``testopia_list`` command lists all package test cases for the update.

Example::

  mtui> testopia_list
  check clock and timezone module         : confirmed (manual)
  https://bugzilla.suse.com/tr_show_case.cgi?case_id=238193

  check keyboard module                   : confirmed (manual)
  https://bugzilla.suse.com/tr_show_case.cgi?case_id=238194

  check language module                   : confirmed (manual)
  https://bugzilla.suse.com/tr_show_case.cgi?case_id=238195


Can I display the test instructions of a Testopia test case?
============================================================

Test case actions can be displayed for given test case IDs with the
``testopia_show`` command.

Example::

  mtui> testopia_show -t 1198469
  Testcase summary: gdk-pixbuf
  Testcase URL: https://bugzilla.suse.com/tr_show_case.cgi?case_id=1198469
  Testcase automated: no
  Testcase status: confirmed
  Testcase requirements:
  Testcase actions:
  1. check the package

  # rpm -V gdk-pixbuf

  2. using tools calling gdk-pixbuf to access picture files:    xbm, png, gif and so on.

    1) verify the eog calling the gdk-pixbuf lib.
    # ldd /usr/bin/eog

     2) using the eog to access pictures.
    # eog <a picture file>


Can I run a QA ctcs2 testsuite within MTUI?
===========================================

The ``testsuite_*`` commands offer several options to run testsuites
and manage the submission to QADB.

Examples
~~~~~~~~

List available testsuites on a refhost::

  mtui> testsuite_list -t kenny.qam.suse.cz
  testsuites on kenny.qam.suse.cz (sles12sp2_module-x86_64):
  test_gzip-run
  test_php-run
  test_tiff-run


Run a specific testsuite::

  mtui> testsuite_run -t moe.qam.suse.cz test_bzip2-run
  moe.qam.suse.cz:~> test_bzip2-testsuite [0]
  INFO: Variable TESTS_LOGDIR is set, logs will be stored in /var/log/qa/SUSE:Maintenance:3601:126030/ctcs2.
  Initializing test run for control file qa_bzip2.tcf...
  Current time: Thu Apr 20 11:26:02 CEST 2017
  **** Test in progress ****
  qa_bzip2_validation ... ... PASSED (2s)
  qa_bzip2_bigfilerun ... ... PASSED (3s)
  qa_bzip2_bznew ... ... PASSED (1s)
  qa_bzip2_compile ... ... PASSED (1s)
  **** Test run complete ****
  Current time: Thu Apr 20 11:26:09 CEST 2017
  Exiting test run..
  Displaying report...
  Total test time: 7s
  Tests passed:
  qa_bzip2_bigfilerun qa_bzip2_bznew qa_bzip2_compile qa_bzip2_validation
  **** Test run completed successfully ****

  info: done


Submit the testsuite results to QADB::

  mtui> testsuite_submit -t moe.qam.suse.cz test_bzip2-run
  info: Submiting results of test_bzip2-run from moe.qam.suse.cz
  info: submission for moe.qam.suse.cz (sles12_module-x86_64): http://qadb2.suse.de/qadb/submission.php?submission_id=494079
  info: done


Where are stored installation logs from refhosts?
=================================================

Now are stored in template dir / RRID / install_logs

Example::

  tester@khorne ~/qam/SUSE:Maintenance:4769:132999  $ tree
  .
  ├── install_logs
  │   ├── dsdd
  │   ├── hayley.qam.suse.cz.log
  │   ├── s390ctc045.suse.de.log
  │   └── steve.qam.suse.cz.log
  ├── log
  ├── output
  │   └── scripts
  │       ├── post.check_from_same_srcrpm.hayley.qam.suse.cz
  │       ├── post.check_from_same_srcrpm.s390ctc045.suse.de
  │       ├── post.check_from_same_srcrpm.steve.qam.suse.cz
  │       ├── post.check_initrd_state.hayley.qam.suse.cz
  │       ├── post.check_initrd_state.s390ctc045.suse.de
  │       ├── post.check_initrd_state.steve.qam.suse.cz
  │       ├── post.check_new_dependencies.hayley.qam.suse.cz
  │       ├── post.check_new_dependencies.s390ctc045.suse.de
  │       ├── post.check_new_dependencies.steve.qam.suse.cz
  │       ├── post.check_vendor_and_disturl.hayley.qam.suse.cz
  │       ├── post.check_vendor_and_disturl.s390ctc045.suse.de
  │       ├── post.check_vendor_and_disturl.steve.qam.suse.cz
  │       ├── pre.check_from_same_srcrpm.hayley.qam.suse.cz
  │       ├── pre.check_from_same_srcrpm.s390ctc045.suse.de
  │       ├── pre.check_from_same_srcrpm.steve.qam.suse.cz
  │       ├── pre.check_initrd_state.hayley.qam.suse.cz
  │       ├── pre.check_initrd_state.s390ctc045.suse.de
  │       ├── pre.check_initrd_state.steve.qam.suse.cz
  │       ├── pre.check_new_dependencies.hayley.qam.suse.cz
  │       ├── pre.check_new_dependencies.s390ctc045.suse.de
  │       ├── pre.check_new_dependencies.steve.qam.suse.cz
  │       ├── pre.check_vendor_and_disturl.hayley.qam.suse.cz
  │       ├── pre.check_vendor_and_disturl.s390ctc045.suse.de
  │       └── pre.check_vendor_and_disturl.steve.qam.suse.cz
  ├── packages-list.txt
  ├── packages.xml
  ├── patchinfo.xml
  ├── project.xml
  ├── repositories.xml
  ├── scripts
  │   ├── compare
  │   │   ├── compare_from_same_srcrpm.sh
  │   │   ├── compare_initrd_state.sh
  │   │   ├── compare_new_dependencies.sh
  │   │   └── compare_vendor_and_disturl.sh
  │   ├── post
  │   │   ├── check_from_same_srcrpm.pl
  │   │   ├── check_initrd_state.sh
  │   │   ├── check_new_dependencies.sh
  │   │   └── check_vendor_and_disturl.pl
  │   └── pre
  │       ├── check_from_same_srcrpm.pl
  │       ├── check_initrd_state.sh
  │       ├── check_new_dependencies.sh
  │       └── check_vendor_and_disturl.pl
  └── source.diff
