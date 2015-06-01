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
enabled.
Even a command history and history searching are available.
For a short overview of procedures and help texts, the "help" command is
also available (i.e. help add_host prints a short description of the
`add_host`_ command).
Running processes can be interrupted by pressing CTRL-C.
However, it blocks until all currently running commands have finished.


Common Argument Types
=====================

`attribute`
  refhost attribute like architecture, product, `system-type`
  or `hostname`
`hostname`
  one of the hostnames in the target host list; where it makes sense,
  "all" can be used as a proxy for the full target host list
`system-type`
  sles11sp1-i386

Commands
========

Host Management
***************

add_host
++++++++

::

  add_host <hostname>,<system-type>

Register the (`<hostname>`, `<system-type>`) tuple
in the target host list.

remove_host
+++++++++++

::

  remove_host <hostname>[,...]

Disconnect from given refhost(s) and remove it/them from target host
list.

.. warning::
  The host log is purged as well.

If the tester wants to preserve the log, it's better to use
the `set_host_state`_ command instead and set the host to "disabled".

autoadd
+++++++

::

  autoadd <attribute> [...]

Add a refhost matching all given `<attribute>`\s to the target host list.

search_hosts
++++++++++++

::

  search_hosts <attribute> [<attribute>] ...

Search hosts by by the specified attributes.

list_hosts
++++++++++

::

  list_hosts

Lists all connected hosts including the system types and their current
state.  State could be "Enabled", "Disabled" or "Dryrun".

list_history
++++++++++++

::

  list_history [<hostname>,...][,<event>]

Lists a history of mtui events on the target hosts like installing or
updating packages. Date, username and event is shown.  Events can be
filtered with the `<event>` parameter.

`<event>`
  `connect`, `disconnect`, `install`, `update`, `downgrade`

list_locks
++++++++++

::

  list_hosts

Lists lock state of all connected hosts

set_host_state
++++++++++++++

::

  set_host_state <hostname>[,...],<state>

Sets the host state to "Enabled", "Disabled" or "Dryrun". A host set to
"Enabled" runs all issued commands while a "Disabled" host or a host set
to "Dryrun" doesn't run any command on the host. The difference between
"Disabled" and "Dryrun" is that on "Dryrun" hosts the issued commands
are printed to the console while "Disabled" doesn't print anything.
Additionally, the execution mode of each host could be set to "parallel"
(default) or "serial". All commands which are designed to run in
parallel are influenced by this option (like to run command)

`<state>`
  `enabled`, `disabled`, `dryrun`, `parallel`, `serial`

set_host_lock
+++++++++++++

::

    set_host_lock <hostname>[,...],<state>

Lock host for exclusive usage. This locks all repository transactions
like enabling or disabling the testing repository on the target hosts.
The Hosts are locked with a timestamp, the UID and PID of the session.
This influences the update process of concurrent instances, use with
care.  Enabled locks are automatically removed when exiting the session.
To lock the `run`_ command on other sessions as well, it's necessary to
set a comment.

`<state>`
  `enabled`, `disabled`

set_timeout
+++++++++++

::

    set_timeout <hostname>,<seconds>

Changes the current execution timeout for a target host. When the
timeout limit was hit the user is asked to wait for the current command
to return or to proceed with the next one.
To disable the timeout set it to "0".

list_timeout
++++++++++++

::

    list_timeout

Prints the current timeout values per host in seconds.

unlock
++++++

::

  unlock [-f|--force] [<hostname>...]

Unlock given (or all) targets.

`-f`, `--force`
  Remove locks set by other users or sessions.


Update Management
*****************

install
+++++++

::

    install <hostname>[,...],<package>[ <package>]...

Installs packages from the current active repository.
The repository should be set with the `set_repo`_ command beforehand.

uninstall
+++++++++

::

    uninstall <hostname>[,...],<package>[ <package>]...

Removes packages from the system.

prepare
+++++++

::

    prepare <hostname>[,...][,force][,installed][,testing]

Installs missing or outdated packages from the UPDATE repositories.
This is also run by the update procedure before applying the updates.
If "force" is set, packages are forced to be installed on package
conflicts. If "installed" is set, only installed packages are
prepared. If "testing" is set, packages are installed from the TESTING
repositories.

downgrade
+++++++++

::

    downgrade <hostname>

Downgrades all related packages to the last released version (uses
the UPDATE channel). This does not work for SLES 9 hosts, though.

update
++++++

::

    update <hostname>[,newpackage][,noprepare]

Applies the testing update to the target hosts. While updating the
machines, the pre-, post- and compare scripts are run before and after
the update process. If the update adds new packages to the channel, the
"newpackage" parameter triggers the package installation right after the
update.  To skip the preparation procedure, append "noprepare" to the
argument list.

export
++++++

::

    export [<filename>][,<hostname>][,force]

Exports the gathered update data to template file. This includes the
pre/post package versions and the update log. An output file could be
specified, if none is specified, the output is written to the current
testing template.
To export a specific updatelog, provide the hostname as parameter.

`<filename>`
  output template file name
`force`
  overwrite template if it exists

Testing Commands
****************

run
+++

::

    run <hostname>[,...],<command>

Runs a command on specified host(s).  After the call returned, the
output (including the return code) of each host is shown on the console.
Please be aware that no interactive commands can be run with this
procedure.

shell
+++++

::

    shell <hostname>

Invokes a remote root shell on the target host.
The terminal size is set once, but isn't adapted on subsequent changes.

put
+++

::

    put <filename>

Uploads files to all enabled hosts. Multiple files can be selected with
special patterns according to the rules used by the Unix shell (i.e.
``*`` ``?``, ``[]``). The complete filepath on the remote hosts is shown
after the upload.

get
+++

::

    get <filename>

Downloads a file from all enabled hosts. Multiple files can not be
selected.  Files are saved in the `$TEMPLATE_DIR/downloads/`
subdirectory with the hostname as file extension.

set_repo
++++++++

::

    set_repo <hostname>[,...],<repository>

Sets the software repositories to UPDATE or TESTING. `rep-clean.sh`
script is used in the target hosts to set the repositories accordingly.

`<repository>`
  `TESTING` or `UPDATE`

show_log
++++++++

::

    show_log [<hostname>]

Prints the command protocol from the specified hosts. This might be
handy for the tester as well, as one can simply dump the command history
to the reproducer section of the template.

testsuite_run
+++++++++++++

::

    testsuite_run <hostname>[,...],<testsuite>

Runs ctcs2 testsuite and saves logs to `/var/log/qa/$id` on the target
hosts.  Results can be submitted with the `testsuite_submit`_ command.

`<testsuite>`
  testsuite-run command

testsuite_submit
++++++++++++++++

::

    testsuite_submit <hostname>[,...],<testsuite>

Submits the ctcs2 testsuite results to http://qadb.suse.de.
The comment field is populated with some attributes like SWAMPID or
testsuite name, but can also be edited before the results get submitted.
Submitting results to qadb requires the rd-qa NIS password.

`<testsuite>`
  testsuite-run command

testsuite_list
++++++++++++++

::

    testsuite_list <hostname>

List available testsuites on the target hosts.

testopia_list
+++++++++++++

::

    testopia_list [<package>[,...]]

List all Testopia package testcases for the current product.
If given no packages, display testcases for the current update.

`<package>`
  package to display testcases for

testopia_show
+++++++++++++

::

    testopia_show <testcase>[,...]

Show Testopia testcase

`<testcase>`
  testcase ID

testopia_create
+++++++++++++++

::

    testopia_create <package>,<summary>

Create new Testopia package testcase. An editor is spawned to process a
testcase template file.

`<package>`
  package to create testcase for
`<summary>`
  testcase summary

testopia_edit
+++++++++++++

::

    testopia_edit <testcase>

Edit already existing Testopia package testcase. An editor is spawned
to process a testcase template file.

`<testcase>`
  testcase ID

Metadata Commands
*****************

load_template
+++++++++++++

::

    load_template <id>

Load QA Maintenance template by an update identifier.
All changes and logs from an already loaded template are lost if not
saved previously.  Already connected hosts are kept and extended by the
reference hosts defined in the template file.

`<id>`
  either an MD5 hash (SWAMP-using updates), or a SUSE:Maintenance:X:Y
  slug (IBS/SMASH-using updates)

list_metadata
+++++++++++++

::

    list_metadata

Lists patchinfo metadata like patch number, SWAMP ID or packager.

list_bugs
+++++++++

::

    list_bugs

Lists related bugs and corresponding Bugzilla URLs.

list_packages
+++++++++++++

::

    list_packages [-w | <hostname>]

Lists current installed package versions from given (or all) targets.

If -w is specified, all required package versions which should be
installed after the update are listed. If version "None" is shown for
a package, the package is not installed.

list_versions
+++++++++++++

::

    list_versions [<package>[,...]]

Prints the package version history in chronological order.
The history of every test host is checked and consolidated.
If no packages are specified, the version history of the
update packages are shown.

`<package>`
  package name to show version history for

list_update_commands
++++++++++++++++++++

::

    list_update_commands

List all commands which are invoked when applying updates on the target
hosts.

list_downgrade_commands
+++++++++++++++++++++++

::

    list_downgrade_commands

List all commands which are invoked when downgrading packages on the target
hosts.

list_scripts
++++++++++++

::

    list_scripts

List available scripts from the scripts subdirectory. This scripts are
run in a pre updated state and in the post updated state. Afterwards the
corresponding compare scripts are run. The subdirectory
(pre/post/compare) shows in which state the script is run. For more
information, see the User Scripts section in Documentation/README.

add_scripts
+++++++++++

::

        add_scripts <script>[,...]

Add check script from the pre/post testruns.

remove_scripts
++++++++++++++

::

        remove_scripts <script>[,...]

Remove check script from the pre/post testruns.

list_sessions
+++++++++++++

::

    list_sessions <hostname>

Lists current active ssh sessions on target hosts.


Internal Commands
*****************

set_session_name
++++++++++++++++

::

    set_session_name [<name>]

Set optional mtui session name as part of the prompt string.

`<name>`
  session name

set_log_level
++++++++++++++

::

    set_log_level <loglevel>

Changes the current default MTUI loglevel to `<loglevel>`.
The "debug" level can be useful for longer running commands as the
output is shown in realtime.

`<loglevel>`
  `warning`, `info` or `debug`

set_location
++++++++++++

::

    set_location <site>

Change current refhost location to another site.

`<site>`
  location name

save
++++

::

    save [<filename>]

Save the session log to a XML file. All commands and package versions
are saved there. When no parameter is given, the XML is saved to
`$TEMPLATE_DIR/output/log.xml`. If that file already exists and the
tester doesn't want to overwrite it, a postfix (current timestamp) is
added to the filename.  The log can be used to fill the required
sections of the testing template after the testing has finished.

`<filename>`
  save log as file filename

exit, quit
++++++++++

::

    exit [reboot|poweroff]
    quit [reboot|poweroff]

Disconnects from all hosts and exits the program.
The tester is asked to save the XML log when exiting MTUI.

`reboot`
  reboot all target hosts
`poweroff`
  shutdown all target hosts

help
++++

::

    help [<command>]

Prints a short help text for the requested procedure or a list of all
available function if no parameter is given.

`<command>`
  print help text for this MTUI command

report-bug
++++++++++

::

  report-bug [-p|--print-url]

Open mtui bugzilla with fields common for all mtui bugs prefilled

`-p`, `--print-url`
  just print url to the stdout

Other Commands
**************

checkout
++++++++

::

    checkout

Update template files from the SVN.

commit
++++++

::

    commit [<message>]

Commits the testing template to the SVN. This can be run after the
testing has finished an the template is in the final state.

source_extract
++++++++++++++

::

    source_extract [<filename>]

Extracts source RPM to `/tmp`. If no filename is given, the whole
package content is extracted.

`<filename>`
  filename to extract

source_diff
+++++++++++

::

    source_diff <type>

Creates a source diff between the update package and the currently
installed package. If the diff needs to be against the latest
released package, make sure to run "prepare" first.

If `<type>` is "source", a package source diff is created.
This creates usually a diff of the specfile and new patchfiles.

If `<type>` is "build", a build diff is created.
This creates a diff between the patched build directories and
is usually architecture dependend.

The `osc`_ command line client needs to be installed first:
on the MTUI-running host for `source`, on the targets for `build`.

`<type>`
  `build`, `source`

.. _osc: https://build.suse.de/search?search_text=osc

source_verify
+++++++++++++

::

    source_verify

Verifies SPECFILE content. Makes sure that every Patch entry is applied.

source_install
++++++++++++++

::

    source_install <hostname>

Installs current source RPMs to the target hosts.

terms
+++++

::

    terms [<termname>]

Spawn terminal screens to all connected hosts. This command does actually
just run the available helper scripts. If no termname is given, all
available terminal scripts are shown.
Script name should be shell.<termname>.sh
Currently, helper scripts are available for KDE[34], GNOME and xterm.

`<termname>`
  terminal emulator to spawn consoles on

edit
++++

::

    edit file,<filename>
    edit template

Edit a local file, the testing template, the specfile or a patch.
The evironment variable EDITOR is processed to find the prefered
editor. If EDITOR is empty, "vi" is set as default.

`<filename>`
  edit filename
`template`
  edit template
`specfile`
  edit specfile
`patch`
  edit patch

