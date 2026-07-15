.. vim: set tw=72 sts=2 sw=2 et

########################################################################
                                  FAQ
########################################################################

.. contents:: Table of Contents
    :depth: 3

Running MTUI
############

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

  # EDITOR=nano mtui -a SUSE:Maintenance:3601:126030

Is there a way to easily distinguish among different updates?
=============================================================

When you are testing different updates at the same time, you can load several
test reports into a single MTUI session as templates. Each ``load_template``
adds another RRID to the session; ``list_templates`` shows the loaded set,
``switch`` changes the active one, and ``unload`` drops one (closing only its
host connections). The bottom toolbar shows the active template's RRID together
with the total number of loaded templates, so the prompt stays unambiguous.

Action commands fan out across every loaded template by default, each acting on
that template's own hosts (or report), with each template's output prefixed by
an ``=== <RRID> ===`` banner. Scope a single command to one template with
``-T RRID``/``--template RRID``, or force fan-out explicitly with
``--all-templates``. See `load_template`, `list_templates`, `switch`, and
`unload` in the interactive commands reference.

Example::

  mtui> load_template -a SUSE:Maintenance:3601:126030
  mtui> load_template -a SUSE:Maintenance:3602:126040
  mtui> list_templates
  * SUSE:Maintenance:3602:126040  hosts: 2  workflow: manual
    SUSE:Maintenance:3601:126030  hosts: 3  workflow: manual
  mtui> switch SUSE:Maintenance:3601:126030

Which update should I pick up next?
===================================

The ``updates`` command lists the update queue live from the TeReGen API,
sorted by priority. By default it shows the actionable pickup queue:
**unassigned** updates that are **in testing**, so you can grab the next one
without wading through released entries. Each row shows priority, status, kind,
deadline and the RRID.

Pass ``--status all`` for the full queue (every status and assignee), or use
the assignment filters ``--mine`` (updates assigned to you), ``--assignee
<user>``, or ``--all-assignees``. Restrict to a review group with
``--review-group`` and cap the row count with ``--limit``.

Example::

  mtui> updates --review-group qam-sle --limit 5

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

  mtui> load_template -a SUSE:Maintenance:3601:126030
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

If the current test report was already committed to the central
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
do so by using the ``-t`` parameter and adding the hostname to the ``export``
command.

Example::

  mtui> export -t edna.qam.suse.de
  warning: file /suse/testing/testreports//SUSE:Maintenance:3601:126030/log exists.
  should i overwrite /suse/testing/testreports//SUSE:Maintenance:3601:126030/log? (y/N) y
  info: exporting XML to /suse/testing/testreports//SUSE:Maintenance:3601:126030/log
  wrote template to /suse/testing/testreports//SUSE:Maintenance:3601:126030/log

How do I avoid exporting a specific host's results into the test report?
========================================================================

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
installs all packages right after the update, saving you the need to install
these packages manually.

Example::

  mtui> update --newpackage
  info: preparing
  info: done
  info: updating
  info: preparing
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


Can I install all packages from the test update repositories at once?
=====================================================================

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

The ``add_host`` command adds a specific host to the list. 

Example::

  mtui> add_host -t craig.qam.suse.cz
  info: connecting to craig.qam.suse.cz


How can I search the reference-host inventory without connecting?
=================================================================

The ``list_refhosts`` command queries the same inventory ``add_host``
resolves through, but reads it offline: it makes no SSH connection, takes no
lock, and needs no loaded test report. Filter by hostname glob (``--name``),
arch (``--arch``), base product (``--product``), version (``--version``),
addon (``--addon``), or a full testplatform query (``--testplatform``). Add
``--free`` to additionally probe each matched host's live lock state (the only
part that goes on the wire), ``--pool`` to group by test-target slot, or
``--json`` for structured output.

Example::

  mtui> list_refhosts --product sles --version 15-SP6 --arch x86_64


How can I override refhosts mentioned in the test report?
=========================================================

Usually it's sufficient to simply load the hosts from the test report and
add or remove refhosts with the appropriate commands.
For some corner-cases, like exclusive automated testing on virtual
machines, the host list could be overwritten with the ``-s`` option.

Example::

  # mtui -s edna.qam.suse.cz,moe.qam.suse.cz -a SUSE:Maintenance:3601:126030
  info: connecting to edna.qam.suse.cz
  info: connecting to moe.qam.suse.cz
  mtui> list_hosts
  edna.qam.suse.cz     (sles12_module-x86_64): Enabled (parallel)
  moe.qam.suse.cz      (sles12_module-x86_64): Enabled (parallel)
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
  ├── packages-list.txt
  ├── packages.xml
  ├── patchinfo.xml
  ├── project.xml
  ├── repositories.xml
  └── source.diff
