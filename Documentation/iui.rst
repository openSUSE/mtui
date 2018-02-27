.. vim: tw=72 sts=2 sw=2 et

########################################################################
                 Internal (Interactive) User Interface
########################################################################

.. contents::
  :depth: 4

Introduction
============

The MTUI shell is comparable to a bash shell as both use the readline
backend for command processing.

For all shell commands, autocompletion and line editing features are
enabled, as well as command history and history searching.

For a short overview of procedures and help texts, the ``help`` command is
also available (i.e. ``help add_host`` prints a short description of the
`add_host`_ command). Alternatively, you may use the ``--help`` argument (e.g.
``add_host -h`` or ``add_host --help``).

Running processes can be interrupted by pressing CTRL-C.
However, it blocks until all currently running commands have finished.


Common Argument Types
=====================

.. option:: -t HOST, --target HOST

  Address of the target host (should be the FQDN).

  In most cases ``-t`` is an optional argument; can be used multiple times, and
  if omitted, all hosts are used.


Commands
========

Host Management
***************

add_host
++++++++

::

  add_host -t HOST

Adds another machine to the target host list.

Target host need to be specified when adding a host.


remove_host
+++++++++++

::

  remove_host [-t HOST]

Disconnects from given refhost(s) and removes them from the target host list.

.. warning::
  When used without parameters, the command removes all hosts.

  The host log is purged as well.

If the tester wants to preserve the log, the `set_host_state`_ command should be
considered instead, to set the host to ``disabled``.


list_hosts
++++++++++

::

  list_hosts

Lists all connected hosts, including the system types and their current
state: ``enabled``, ``disabled`` or ``dryrun``.


list_history
++++++++++++

::

  list_history [-e EVENT] [-t HOST]

Lists a history of MTUI events on the target hosts, such as installing or
updating packages. Date, username and event is shown. Events can be
filtered with the ``EVENT`` parameter.

**Options:**

.. option:: -e EVENT, --event EVENT

  Event to list: ``connect``, ``disconnect``, ``update``, ``downgrade``, ``install``.


list_locks
++++++++++

::

  list_locks

Lists lock state of all connected hosts.


list_products
+++++++++++++

::

  list_products [-t HOSTS]

Lists installed poducts on selected or all hosts.


reload_products
+++++++++++++++

::

  reload_products [-t HOSTS]

Refresh informations about installed products on selected or all host.


set_host_state
++++++++++++++

::

  set_host_state [-t HOST] state

Sets the host state to ``enabled``, ``disabled`` or ``dryrun``.

A host set to ``enabled`` runs all issued commands, while a ``disabled`` host or
a host set to ``dryrun`` doesn't run any command. The difference between
them is that on ``dryrun`` hosts, the issued commands are printed to the console,
while ``disabled`` doesn't print anything.

Additionally, the execution mode of each host can be set to ``parallel``
(default) or ``serial``. All commands which are designed to run in
parallel (such as the ``run`` command) are influenced by this option.

**Options:**

.. option:: state

  The desired host state: ``enabled``, ``disabled``, ``dryrun``, ``parallel``,
  ``serial``


lock
++++

::

    lock [-t HOST]

Locks host for exclusive usage. This locks all repository transactions, such as
enabling or disabling the testing repository on the target hosts.

.. caution::
  The hosts are locked with a timestamp, the UID and PID of the session.
  This influences the update process of concurrent instances. Use with care.

Enabled locks are automatically removed when exiting the session.
To lock the `run`_ command on other sessions as well, it's necessary to
set a comment.


set_timeout
+++++++++++

::

    set_timeout [-t HOST] timeout

Changes the current execution timeout for a target host. When the
timeout limit is hit, the user is asked to wait for the current command
to return, or to proceed with the next one. The timeout value is set in seconds.
To disable the timeout, set it to "0".

**Options:**

.. option:: timeout

  Timeout in sec; ``0`` disables it.


list_timeout
++++++++++++

::

    list_timeout

Prints the current timeout values per host in seconds.


unlock
++++++

::

    unlock [-f] [-t HOST]

Unlocks given targets. Unlocks all if used without arguments.

**Options:**


.. option:: -f, --force

  Force unlock - removes locks set by other users or sessions.


EOF
+++

::

    EOF [reboot | poweroff]

Reboots or shuts down the refhosts.

**Options:**

.. option:: reboot

  Reboots the refhosts.

.. option:: poweroff

  Shuts down the refhosts.


Update Management
*****************

install
+++++++

::

    install [-t HOST] package [package ...]

Installs packages from the current active repository.
The repository should be set with the `set_repo`_ command beforehand.

**Options:**

.. option:: package

  Package to install.


uninstall
+++++++++

::

    uninstall [-t HOST] package [package ...]

Removes packages from the system.

**Options:**

.. option:: package

  Package to uninstall.


prepare
+++++++

::

    prepare [-f] [-i] [-u] [-t HOST]


Installs missing or outdated packages from the regular UPDATE repositories.

This command is also run by the update procedure before applying the updates.

**Options:**

.. option:: -f, --force

  Forces package installation even on package conflicts.

.. option:: -i, --installed

  Prepares only installed packages.

.. option:: -u, --update

  Enables test update repositories and installs from there.


downgrade
+++++++++

::

    downgrade [-t HOST]

Downgrades all related packages to the last released version (using
the UPDATE channel).

update
++++++

::

    update [--newpackage] [--noprepare] [--noscript] [-t HOST]


Runs the `prepare`_ command and applies the testing update to the target hosts.
(To skip the preparation procedure, use ``--noprepare``.)

While updating the machines, the pre-, post- and compare scripts are run before
and after the update process.
(To skip run of scripts use ``--noscript`` parameter.)

If the update adds new packages to the channel, the "newpackage" parameter
triggers the package installation right after the update.

Update uses internally the products structure from refhost. If this structure was
changed before an `update`_ please use `reload_products`_ command.

**Options:**

.. option:: --newpackage

  Installs new packages after update.

.. option:: --noprepare

  Skips the prepare procedure.

.. option:: --noscript

  Skips the pre- and post- scripts.


export
++++++

::

    export [-f] [-t HOST] [filename]

Exports the gathered update data to template file. This includes the
pre/post package versions and the update log. An output file can be
specified; if none is specified, the output is written to the current
testing template.

Refhost zypper installation logs are exported to subdir per refhost.

**Options:**

.. option:: -f, --force

  Force-overwrites the existing template.

.. option:: filename

  Output template file name.


Testing Commands
****************

run
+++

::

    run [-t HOST] command

Runs a command on a specified host or on all enabled targets.

The command timeout is set to 5 minutes, after which, if there is no output on
stdout or stderr, a timeout exception is thrown. The commands are run in parallel
on every target, or in serial mode when set with ``set_host_state``.

After the call is returned, the output (including the return code) of each host
is shown on the console. Please be aware that no interactive commands can be
run with this procedure.

**Options:**

.. option:: command

  Command to run on refhost.


lrun
++++

::

    lrun command

Runs a command in local shell.

The command runs in the current working directory (where MTUI was started), unless
chroot to the template dir is enabled.

**Options:**

.. option:: command

  Command to run in a local shell.


shell
+++++

::

    shell [-t HOST]

Invokes a remote root shell on the target host.
The terminal size is set once, but isn't adapted on subsequent changes.


put
+++

::

    put filename

Uploads files to all enabled hosts. Multiple files can be selected with
special patterns according to the rules used by the Unix shell (i.e.
``*`` ``?``, ``[]``). The complete filepath on the remote hosts is shown
after the upload.

**Options:**

.. option:: filename

  File to upload to all hosts.


get
+++

::

    get filename

Downloads a file from all enabled hosts. Multiple files cannot be
selected. Files are saved in the ``$TEMPLATE_DIR/downloads/``
subdirectory with the hostname as file extension.
If the argument ends with a slash '/', it will be treated
as a folder and all its contents will be downloaded.

**Options:**

.. option:: filename

  File to download from target hosts.

set_repo
++++++++

::

    set_repo (-A | -R) [-t HOST]

Adds or removes issue repository to/from hosts. It uses ``repose issue-add`` and
``repose issue-rm`` command.

**Options:**

.. option:: -A, --add

  Adds issue repos to refhosts.

.. option:: -R, --remove

  Removes issue repos from refhosts.


show_log
++++++++

::

    show_log [-t HOST]

Prints the command protocol from the specified hosts. This might be
handy for the tester, as one can simply dump the command history
to the reproducer section of the template.

testsuite_run
+++++++++++++

::

    testsuite_run [-t HOST] testsuite

Runs a ctcs2 testsuite and saves logs to ``/var/log/qa/RRID`` on the target
hosts. Results can be submitted with the `testsuite_submit`_ command.

**Options:**

.. option:: testsuite

  Command to execute.


testsuite_submit
++++++++++++++++

::

    testsuite_submit [-t HOST] testsuite

Submits the ctcs2 testsuite results to http://qadb.suse.de.
The comment field is populated with some attributes like RRID or
testsuite name, but can also be edited before the results get submitted.
Submitting results to qadb requires the rd-qa NIS password.

**Options:**

.. option:: testsuite

  Command executed by `testsuite_run`_.


testsuite_list
++++++++++++++

::

    testsuite_list [-t HOST]

Lists available testsuites on the target hosts.


testopia_list
+++++++++++++

::

    testopia_list [-p [PACKAGE]]

Lists all Testopia package testcases for the current product.
If no packages are given, testcases for the current update are displayed.

**Options:**

.. option:: -p [PACKAGE], --package [PACKAGE]

  Package to display testcases for.


testopia_show
+++++++++++++

::

    testopia_show -t TESTCASE

Shows a specified Testopia testcase.

**Options:**

.. option:: -t TESTCASE, --testcase TESTCASE

  Testcase to show.


testopia_create
+++++++++++++++

::

    testopia_create package summary

Creates a new Testopia package testcase. An editor is spawned to process a
testcase template file.

**Options:**

.. option:: package

  Package to create a testcase for.

.. option:: summary

  Summary of the testcase.


testopia_edit
+++++++++++++

::

    testopia_edit testcase_id

Edits an already existing Testopia package testcase. An editor is spawned
to process a testcase template file.

**Options:**

.. option:: testcase_id

  Testcase ID of the testcase to edit.


Metadata Commands
*****************

load_template
+++++++++++++

::

    load_template [-c] update_id

Loads a QA Maintenance template by its RRID identifier. All changes and logs
from an already loaded template are lost if not saved previously. Already
connected hosts are kept and extended by the reference hosts defined in the
template file.

**Options:**

.. option:: -c, --clean-hosts

  Cleans up old hosts.

.. option:: update_id

  OBS request ID for the update.


list_metadata
+++++++++++++

::

    list_metadata

Lists patchinfo metadata such as patch number, Review Request ID or packager.


list_bugs
+++++++++

::

    list_bugs

Lists related bugs and corresponding Bugzilla URLs.


list_packages
+++++++++++++

::

    list_packages [-p PACKAGE] [-w] [-t HOST]

Lists current installed package versions from given (or all) targets.

If -w is specified, all required package versions which should be
installed after the update are listed. If version "None" is shown for
a package, the package is not installed.

**Options:**

.. option:: -p PACKAGE, --package PACKAGE

  Package to list. Can be used multiple times to query more packages at once.

.. option:: -w, --wanted

  Prints versions required after the update.


show_update_repos
+++++++++++++++++

::
  
    show_update_repos

List all update repositories by Product, version and architecture



list_versions
+++++++++++++

::

    list_versions [-p PACKAGE] [-t HOST]

Prints the package version history in chronological order.
The history of every test host is checked and consolidated.
If no packages are specified, the version history of the
update packages are shown.

**Options:**

.. option::  -p PACKAGE, --package PACKAGE

  Package name to show the version history for.


list_update_commands
++++++++++++++++++++

::

    list_update_commands

List all commands which are invoked when applying updates on the target
hosts.


list_sessions
+++++++++++++

::

    list_sessions [-t HOST]

Lists current active ssh sessions on target hosts.


Internal Commands
*****************

set_session_name
++++++++++++++++

::

    set_session_name [name]

Set optional mtui session name as part of the prompt string. This can help
finding the correct mtui session if multiple sessions are active.

When no specific name is given, the name is set to the RRID slug
(SUSE:Maintenance:XXXX:YYYYYY).

**Options:**

.. option:: name

  Name of the session.


set_log_level
++++++++++++++

::

    set_log_level loglevel

Changes the current MTUI log level to ``info``, ``error``, ``warning`` or
``debug``.
The ``debug`` level enables debug messages with the output being shown in realtime,
and thus can be especially useful for longer running commands.

.. caution::
  The ``warning`` level only prints basic error or warning conditions,
  therefore is not recommended.

**Options:**

.. option:: loglevel

  Log level of MTUI: ``warning``, ``info`` or ``debug``

set_location
++++++++++++

::

    set_location site

Changes current refhost location to another site.

**Options:**

.. option:: site

  Location name.


config
++++++

::

    config show

Displays MTUI configuration values.

In future versions of MTUI, the ``config`` command will also allow the user to
manipulate config values in runtime.

**Options:**

.. option:: show

  Shows config values.


save
++++

::

    save [filename]

Saves the session log (all commands and package versions) to an XML file.
When no parameter is given, the XML is saved to ``$TEMPLATE_DIR/output/log.xml``.
If that file already exists and the tester doesn't want to overwrite it, a
postfix (current timestamp) is added to the filename.

The log can be used to facilitate filling the required sections of the testing
template after the testing has finished.

**Options:**

.. option:: filename

  Name of the file to save log as.


exit, quit, EOF
+++++++++++++++

::

    exit [reboot|poweroff]
    quit [reboot|poweroff]

Disconnects from all hosts and exits the program.
The tester is asked to save the XML log when exiting MTUI.

.. tip:: Ctrl+D works too.

**Options:**

.. option:: reboot

  Reboots all target hosts.

.. option:: poweroff

  Shuts down all target hosts.


help
++++

::

    help [command]

Prints a short help text for the requested procedure or a list of all
available commands if no parameter is given.

**Options:**

.. option:: command

  The MTUI command to print help for.


report-bug
++++++++++

::

  report-bug [-p]

Opens bugzilla with pre-populated fields relevant for all MTUI bugs.

**Options:**

.. option:: -p, --print-url

  Just prints the bugzilla url to the stdout, without opening the bug editor.


whoami
++++++

::

    whoami

Displays current user name and session PID.


OSC-wrapper Commands
*********************

assign
++++++

::

    assign [-h] [-g [GROUP]]

Wrapper around the `osc qam assign`_ command; assigns current update.
QA groups for assignment can be specified.

.. _osc qam assign: http://qam.suse.de/projects/oscqam/latest/workflows/tester.html#assigning-updates

**Options:**

.. option:: -g [GROUP], --group [GROUP]

  QA group to assign under.


unassign
++++++++

::

    unassign [-h] [-g [GROUP]]

Wrapper around the `osc qam unassign`_ command; unassigns current update.
QA groups for unassignment can be specified.

.. _osc qam unassign: http://qam.suse.de/projects/oscqam/latest/workflows/tester.html#unassigning-updates

**Options:**

.. option:: -g [GROUP], --group [GROUP]

  QA group to unassign under.


approve
+++++++

::

    approve [-h] [-g [GROUP]]

Wrapper around the `osc qam approve`_ command; approves current update. It is
possible to specify more QA groups for approval.

.. _osc qam approve: http://qam.suse.de/projects/oscqam/latest/workflows/tester.html#approve

**Options:**

.. option:: -g [GROUP], --group [GROUP]

  QA group to approve under.


reject
++++++

::

    reject [-h] [-g [GROUP]] -r REASON [-m ...]

Wrapper around the `osc qam reject`_ command; rejects current update. The ``-r``
option is required.

.. _osc qam reject: http://qam.suse.de/projects/oscqam/latest/workflows/tester.html#reject

**Options:**

.. option:: -g [GROUP], --group [GROUP]

  QA group to approve under.

.. option:: -r REASON, --reason REASON

  Reason for rejection: ``admin``, ``retracted``, ``build_problem``,
  ``not_fixed``, ``regression``, ``false_reject``, ``tracking_issue``.

.. option:: -m MESSAGE, --msg MESSAGE

  Message/comment to use for the rejection. Should be always given as the last
  part of the command.


Other Commands
**************

checkout
++++++++

::

    checkout

Updates template files from the SVN.


commit
++++++

::

    commit [-m MESSAGE]

Commits the testing template to the SVN. This can be run after the
testing has finished and the template is in the final state.

**Options:**

.. option:: -m MESSAGE, --msg MESSAGE

  Commit message.


terms
+++++

::

    terms [-t HOST] [termname]

Spawns terminal screens to specified hosts (or to all connected hosts, if no
HOST parameter is given). This command actually just runs the available helper
scripts. If no termname is given, all available terminal scripts are shown.

Script name should be shell.<termname>.sh
Currently, helper scripts are available for gnome-terminal (``gnome``), konsole
(``kde``), xterm, tmux, and urxvtc.

**Options:**

.. option:: termname

  Terminal emulator to spawn consoles on.


edit
++++

::

    edit [filename]

Edits the testing template or a local file. To edit template call ``edit``
without parameters.

The environment variable ``EDITOR`` is processed to find the preferred
editor. If ``EDITOR`` is empty, ``vi`` is set as default.

**Options:**

.. option:: filename

  File to edit.
