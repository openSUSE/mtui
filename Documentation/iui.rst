.. vim: tw=72 sts=2 sw=2 et

########################################################################
                 Internal (Interactive) User Interface
########################################################################

.. contents::
  :depth: 4

Introduction
============

The MTUI shell is a ``prompt_toolkit``-based REPL. Command names and
their arguments complete with TAB against the live command registry;
known command tokens are highlighted by a syntax lexer as you type.
Persistent shell history is shared across sessions and is reachable
through reverse-incremental search (Ctrl-R) and forward search
(Ctrl-S); a fish-style autosuggestion shows the closest history match
in dim text and can be accepted with the right-arrow key.

A bottom toolbar reflects session state: the current workflow mode,
the number of connected reference hosts, the count of loaded test
report templates, and the RRID of the active template (``-`` when none
is loaded), so the prompt stays unambiguous when several mtui
sessions are open in different terminals.

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

.. option:: -T RRID, --template RRID

  Scope a fan-out command to a single loaded template. Only meaningful for
  action commands that fan out (see `Fan-out across templates`_). Mutually
  exclusive with ``--all-templates``.

.. option:: --all-templates

  Force a command to act on every loaded template. This is already the default
  for fan-out commands; the flag is useful for clarity and in scripts. Mutually
  exclusive with ``-T``/``--template``.


Fan-out across templates
========================

When more than one template is loaded (see `load_template`_ and
`list_templates`_), action commands fan out across **all** loaded templates by
default, each acting on that template's own hosts (or that template's own
report, for report-scoped commands). The fan-out commands are: ``run``,
``update``, ``prepare``, ``install``, ``uninstall``, ``downgrade``, ``export``,
``set_repo``, ``reboot``, ``put``, ``get``, ``commit``, ``checkout``,
``approve``, ``assign``, ``unassign``, ``reject``, ``comment``, ``show_diff``,
``analyze_diff``, ``reload_openqa``, ``openqa_overview``, ``openqa_jobs``,
``checkers``, ``set_workflow``, the report-bound inspection commands
``list_metadata``, ``list_bugs``, ``list_update_commands``, ``list_versions``,
``list_packages``, and ``show_update_repos``, the host-management commands
``add_host``, ``remove_host``, ``lock`` and ``unlock``, and the host-listing
commands ``list_hosts``, ``list_locks``, ``list_timeout``, ``list_sessions``,
``show_log``, and ``list_history``. Output for each template is prefixed with an
``=== <RRID> ===`` banner so results stay attributable. The queue-browsing
``updates`` command is not template-scoped and does not fan out. ``regenerate``
acts on the active template by default (use ``-T``/``--all-templates`` to
select).

Use ``-T RRID``/``--template RRID`` to run such a command against a single
loaded template instead, or ``--all-templates`` to request fan-out explicitly.
Navigation and single-target commands (for example `load_template`_, ``edit``,
`switch`_, `unload`_, ``quit``, `list_templates`_) always act on the active
template only.

If a fanned-out command fails on one template, it continues running on the
remaining templates and then reports an aggregate failure once the loop is done;
a failure on one template does not abort the others. When a host-phase command
(one accepting ``-t``) fans out without explicit ``-t`` hosts, a loaded
template that has **no connected host** is skipped with a warning rather than
counted as a failure, so an unscoped ``lock``/``run``/… still succeeds on the
templates that do have hosts. If *every* template gets skipped that way the
command executed nowhere and fails with "No refhosts defined". Naming a host
that is not connected (``-t <host>``) is still a per-template failure, and an
explicitly ``-T``/``--template``-scoped call keeps the single-template error
behaviour — you asked for exactly that one.

.. note::

   Fan-out always draws a **distinct** free reference host per test-target slot
   (product + version + arch + addons) from the shared pool, arbitrated
   in-process so two templates never collide on the same host. When every
   candidate for a slot is busy, the claim queues on an exhausted pool per
   ``lock.wait`` / ``lock.wait_poll``. Scope a command with ``-T`` to act on a
   single template.


Commands
========

Host Management
***************

add_host
++++++++

::

  add_host [-t HOST] [-k] [-T RRID | --all-templates]

Adds another machine to the target host list.
Without parameter adds all hosts from testplatform.

When more than one template is loaded this fans out across all of them by
default (see `Fan-out across templates`_), adding the host(s) to each template's
own host list. Use ``-T RRID`` to scope to a single template.

When adding hosts from a testplatform, mtui connects **one** reference host
per test-target slot (product + version + arch + addons) rather than every
matching candidate; so a testplatform that resolves to several hosts on the
same architecture no longer fans out across all of them. The host drawn for
each slot is chosen at random among the free candidates (load is spread across
interchangeable refhosts). If the chosen host fails to connect, mtui
automatically tries another candidate from the same slot; only when every
candidate for a slot is unreachable is a single warning logged and that slot
left unconnected. The same one-per-slot selection and backup applies to the
autoconnect that runs when a template is loaded.

If the session is in automatic mode, running ``add_host`` switches it to
the manual workflow (adding hosts by hand is a manual action), updating
the prompt accordingly. Pass ``-k``/``--keep-mode`` to add a host without
switching the workflow.

When a host connects, mtui checks that the products actually installed on
it (from ``/etc/products.d``) match what ``refhosts.yml`` records for that
host. Any drift (a wrong or wrong-version base product, a wrong
architecture, addons that are missing, unexpected, or at a different
version, or a dangling ``/etc/products.d/baseproduct`` symlink) is logged
as a ``WARNING`` and the host is kept (the check never aborts a connect).
The ``qa`` product is ignored, and hosts not listed in ``refhosts.yml`` are
skipped silently.


remove_host
+++++++++++

::

  remove_host [-t HOST] [-T RRID | --all-templates]

Disconnects from given refhost(s) and removes them from the target host list.

When more than one template is loaded this fans out across all of them by
default (see `Fan-out across templates`_), removing the host(s) from each
template's own host list. Use ``-T RRID`` to scope to a single template.

.. warning::
  When used without parameters, the command removes all hosts.

  The host log is purged as well.

If the tester wants to preserve the log, the `set_host_state`_ command should be
considered instead, to set the host to ``disabled``.


list_hosts
++++++++++

::

  list_hosts [-T RRID | --all-templates]

Lists all connected hosts, including the system types and their current
state: ``enabled``, ``disabled`` or ``dryrun``.

When more than one template is loaded this fans out across all of them by
default (see `Fan-out across templates`_). Use ``-T RRID`` to scope to a
single template.


list_history
++++++++++++

::

  list_history [-e EVENT] [-t HOST] [-T RRID | --all-templates]

Lists a history of MTUI events on the target hosts, such as installing or
updating packages. Date, username and event is shown. Events can be
filtered with the ``EVENT`` parameter.

When more than one template is loaded this fans out across all of them by
default (see `Fan-out across templates`_). Use ``-T RRID`` to scope to a
single template.

**Options:**

.. option:: -e EVENT, --event EVENT

  Event to list: ``connect``, ``disconnect``, ``update``, ``downgrade``, ``install``.


list_locks
++++++++++

::

  list_locks [-p] [-T RRID | --all-templates]

Lists lock state of all connected hosts.

By default only the zypper/operation locks (set by ``lock`` and the
install/update/prepare/downgrade flows) are shown. Use ``-p``/``--pool`` to
instead list the host *pool* claims taken during pool selection. The two
lock mechanisms are independent and use separate lock files on the hosts.

When more than one template is loaded this fans out across all of them by
default (see `Fan-out across templates`_). Use ``-T RRID`` to scope to a
single template.


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


list_refhosts
+++++++++++++

::

  list_refhosts [-T QUERY] [-n GLOB] [-a ARCH] [-p PRODUCT]
                [--version VERSION] [--addon ADDON]
                [--pool] [--json] [--free] [-v]

Queries and searches the reference-host inventory **offline**: no SSH
connection, no lock, and no loaded test report. It reads the same source
`add_host`_ resolves through (``RefhostsFactory``), so fleet maintenance and
manual users can find refhosts through mtui instead of parsing
``refhosts.yml`` by hand.

With no filters, every known refhost is listed; results are de-duplicated by
host name. Only ``--free`` goes on the wire; it probes each matched host's
live mtui-lock state.

**Options:**

.. option:: -T QUERY, --testplatform QUERY

  Match a SMELT testplatform query, e.g.
  ``base=sles(major=15,minor=6);arch=[x86_64]``.

.. option:: -n GLOB, --name GLOB

  Hostname glob, e.g. ``whale-*`` or ``*.qam.suse.cz``.

.. option:: -a ARCH, --arch ARCH

  Architecture filter: ``x86_64``, ``aarch64``, ``ppc64le`` or ``s390x``.
  Can be used multiple times.

.. option:: -p PRODUCT, --product PRODUCT

  Base-product substring, e.g. ``sles``, ``sled`` or ``SLE_HPC``.

.. option:: --version VERSION

  Product version: ``15-SP6``, ``15.6`` or ``15`` (SP optional).

.. option:: --addon ADDON

  Addon-name substring. Can be used multiple times.

.. option:: --pool

  Group the result by test-target slot (product, version, arch and addons).

.. option:: --json

  Emit structured JSON instead of the aligned table.

.. option:: --free

  Also probe each matched host's live mtui-lock state. This is the only part
  of the command that connects to the hosts.

.. option:: -v, --verbose

  Include addons in the output.


reload_openqa
+++++++++++++

::
  
  reload_openqa

Reload informations from openQA instances.


openqa_overview
+++++++++++++++

::

  openqa_overview [--no-aggregated] [--days N]
                  [--aggregated-groups {core,containers,yast,security} [...]]
                  [--url-openqa URL] [--url-dashboard-qam URL] [--url-qam URL]
                  [--test-pattern REGEX] [--export] [--no-fetch]

Port of the ``oqa-search`` helper script
(https://github.com/mjdonis/oqa-search).

For the currently loaded testreport, prints three sections suitable for
pasting into the update log:

* **Single Incidents - Core**: PASSED / FAILED / RUNNING per SLE
  version.
* **Aggregated Updates**: most recent build per requested group
  (default ``core``) within the last ``--days`` days that exercises the
  current incident.
* **Build checks**: parsed test-result summaries scraped from the
  qam.suse.de ``build_checks/`` directory.

URLs default to mtui's config (``openqa_instance``,
``qem_dashboard_api``, ``reports_url``). The structured payload is
stored on the testreport at ``metadata.openqa.overview`` for later
reuse.


.. option:: --no-aggregated

  Skip the Aggregated Updates section.

.. option:: --days N

  How many days to walk back when searching for aggregated builds
  (1-30, default 5).

.. option:: --aggregated-groups GROUP [GROUP ...]

  Aggregated job groups to query. One or more of
  ``core``, ``containers``, ``yast``, ``security``. Default: ``core``.

.. option:: --url-openqa URL

  Override the openQA host (otherwise ``config.openqa_instance``).

.. option:: --url-dashboard-qam URL

  Override the QAM Dashboard base URL (otherwise derived from
  ``config.qem_dashboard_api``).

.. option:: --url-qam URL

  Override the QAM base URL (otherwise derived from
  ``config.reports_url``).

.. option:: --test-pattern REGEX

  Custom regex applied to each build-check log instead of the default
  heuristics.

.. option:: --export

  Also inject the overview into the loaded testreport's ``log`` file
  under the ``regression tests:`` section. The inserted block is
  delimited by ``<!-- mtui openqa_overview begin -->`` /
  ``<!-- mtui openqa_overview end -->`` markers so re-exports replace
  the prior block in place instead of duplicating it. The regular
  ``export`` command will also pick up
  ``metadata.openqa.overview`` automatically when it runs, so an
  earlier ``openqa_overview`` (without ``--export``) followed by
  ``export`` produces the same result.

.. option:: --no-fetch

  Skip the network search and reuse the cached overview from
  ``metadata.openqa.overview``. Only meaningful with ``--export``;
  a no-op when nothing is cached (the command then falls back to a
  normal fetch).


openqa_jobs
+++++++++++

::

  openqa_jobs [--all] [--failed] [--arch ARCH]
              [--url-openqa URL] [--url-dashboard-qam URL]

Lists the **individual** openQA jobs for the loaded update's incident build,
so you can see *which* scenarios passed or failed (and judge whether a failure
relates to the package under test) rather than only the per-version
PASSED/FAILED/RUNNING summary that `openqa_overview`_ prints. Prints a per-result
count summary followed by one colourised line per job (``result``, ``arch``,
scenario, job URL).

By default ``obsoleted`` jobs (superseded by a later retrigger) are dropped;
only the current run matters.

**Options:**

.. option:: --all

  Include ``obsoleted`` (superseded) jobs.

.. option:: --failed

  Show only non-passing jobs (``failed`` / ``parallel_failed`` / ``incomplete``;
  ``passed``/``softfailed``/``skipped``/``obsoleted`` are hidden).

.. option:: --arch ARCH

  Only jobs for this architecture.

.. option:: --url-openqa URL

  Override the openQA host (otherwise ``config.openqa_instance``).

.. option:: --url-dashboard-qam URL

  Override the QAM Dashboard base URL (otherwise derived from
  ``config.qem_dashboard_api``).


checkers
++++++++

::

    checkers [-T RRID | --all-templates]

Prints the build-check (checker) result runs for the loaded SLFO update, fetched
live from the TeReGen report API (``GET /reports/{id}/checkers``): per run the
checker name and its pass/fail status. SLFO only. Report-bound -- fans out across
loaded templates (see `Fan-out across templates`_). This is the
TeReGen-backed replacement for the former ``smelt_checkers`` command (same data,
now served through TeReGen instead of SMELT directly). Requires the TeReGen base
URL in ``[teregen] api`` (see :doc:`cfg`).


updates
+++++++

::

    updates [--review-group GROUP] [--status STATUS] [--limit N]
            [--assignee USER | --mine | --all-assignees]

Enumerates the update queue, fetched live from the TeReGen API
(``GET /updates``, fed from SMELT behind the scenes), one line per update with
its priority, status, kind, deadline and RRID. This is the TeReGen-backed
replacement for the former ``smelt_updates`` / ``smelt_requests`` commands. Not
template-scoped; does not fan out. Requires the TeReGen base URL in
``[teregen] api`` (see :doc:`cfg`).

By default the command shows the actionable pickup queue: **unassigned**
updates that are **in testing**. Choosing another assignment view
(``--assignee``/``--mine``/``--all-assignees``) drops the unassigned default,
and ``--status all`` widens the queue to every status and assignee (including
released updates).

**Options:**

.. option:: --review-group GROUP

  Narrow to updates assigned to this review group (e.g. ``qam-sle``).

.. option:: --status STATUS

  Only updates with this status. Defaults to ``testing``; pass ``all`` to show
  every status (including released updates).

.. option:: --limit N

  Cap the number of rows (``0`` = all).

.. option:: --assignee USER

  Filter to updates assigned to ``USER`` (any qam group). Drops the unassigned
  default.

.. option:: --mine

  Filter to updates assigned to the current session user. Drops the unassigned
  default.

.. option:: --all-assignees

  Show every update regardless of assignee (assigned and unassigned), with the
  assignee on each row. Overrides the unassigned default.


regenerate
++++++++++

::

    regenerate [--force] [--ignore-inconsistent] [--no-wait]
               [-T RRID | --all-templates]

Regenerates the loaded update's test-report template via the TeReGen API
(``POST /reports/{id}/regenerate``), waits for the Minion generation job to
finish, then reloads the freshly generated template into the session. Use it to
pick up a regenerated template -- e.g. after an arch/metadata fix -- without
leaving and re-entering mtui. Acts on the active template by default. Requires the
TeReGen base URL in ``[teregen] api`` (see :doc:`cfg`).

The stale-template loader makes the same offer automatically: when a
checked-out template's hash no longer matches its Gitea PR, mtui offers to
regenerate via TeReGen, delete the local checkout, and wait for the rebuild
before reloading it in place.

**Options:**

.. option:: --force

  Overwrite an existing but *unedited* template. (The server still refuses to
  overwrite a hand-edited template.)

.. option:: --ignore-inconsistent

  Regenerate despite inconsistent metadata, e.g. an arch list that disagrees
  with the build.

.. option:: --no-wait

  Enqueue the regeneration job and return immediately, without waiting for it to
  finish or reloading the template.


set_workflow
++++++++++++

::
  
  set_workflow {auto,manual,kernel} [-T RRID | --all-templates]

Sets workflow and reload data from openQA.

'auto' workflow will be automatically set to manual if openQA install tests
missing or have failed state

When more than one template is loaded this fans out across all of them by
default (see `Fan-out across templates`_), reloading openQA data for each. Use
``-T RRID`` to scope to a single template.

.. option:: workflow

  one of supported workflows 


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

    lock [-t HOST] [-c COMMENT] [-T RRID | --all-templates]

Locks host for exclusive usage. This locks all repository transactions, such as
enabling or disabling the testing repository on the target hosts.

When more than one template is loaded this fans out across all of them by
default (see `Fan-out across templates`_). Use ``-T RRID`` to scope the lock to
a single template.

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

    list_timeout [-T RRID | --all-templates]

Prints the current timeout values per host in seconds.

When more than one template is loaded this fans out across all of them by
default (see `Fan-out across templates`_). Use ``-T RRID`` to scope to a
single template.


unlock
++++++

::

    unlock [-f] [-p] [-t HOST] [-T RRID | --all-templates]

Unlocks given targets. Unlocks all if used without arguments.

By default this removes the zypper/operation lock. Use ``-p``/``--pool`` to
instead remove the host *pool* claim. The two lock mechanisms are
independent and use separate lock files on the hosts.

When more than one template is loaded this fans out across all of them by
default (see `Fan-out across templates`_). Use ``-T RRID`` to scope the unlock
to a single template.

**Options:**


.. option:: -f, --force

  Force unlock - removes locks set by other users or sessions (or, with
  ``--pool``, by other templates).


.. option:: -p, --pool

  Remove the pool claim instead of the zypper/operation lock.


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

reboot
++++++

::

    reboot [-t HOST]

Reboots reference hosts and reconnects once they are back up. With no
argument all connected reference hosts are rebooted; ``-t``/``--target``
limits it to the named hosts. The reboot is dispatched without waiting
(the SSH connection is expected to drop) and mtui reconnects
automatically with retries and backoff. Works for both transactional and
non-transactional hosts.

While testing a Product Increment, the per-host testing lock is
re-applied after the reboot (a reboot clears ``/var/lock``), so it is not
lost.

update
++++++

::

    update [--newpackage] [--noprepare] [-t HOST]


Runs the `prepare`_ command and applies the testing update to the target hosts.
(To skip the preparation procedure, use ``--noprepare``.)

If the update adds new packages to the channel, the "newpackage" parameter
triggers the package installation right after the update.

Update uses internally the products structure from refhost. If this structure was
changed before an `update`_ please use `reload_products`_ command.

**Options:**

.. option:: --newpackage

  Installs new packages after update.

.. option:: --noprepare

  Skips the prepare procedure.


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

  Force overwrite existing template and if openQA results are in log,
  download them again and replace older records.

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

    show_log [-t HOST] [-T RRID | --all-templates]

Prints the command protocol from the specified hosts. This might be
handy for the tester, as one can simply dump the command history
to the reproducer section of the template.

When more than one template is loaded this fans out across all of them by
default (see `Fan-out across templates`_). Use ``-T RRID`` to scope to a
single template.

Metadata Commands
*****************

load_template
+++++++++++++

::

    load_template (-a RequestReviewID | -k RequestReviewID)

Loads a QA Maintenance template by its RRID identifier. The template is
*added* to the session: previously loaded templates stay loaded and the newly
loaded one becomes active. Loading an RRID that is already loaded reloads and
replaces its stored report (and makes it active). Each loaded template owns its
own reference hosts: the newly loaded template connects only the hosts defined
in its own template file (or selected from its testplatforms) and never
reconnects hosts that belong to another loaded template. `-a` and `-k` options
are mutually exclusive.

Use `list_templates`_ to see all loaded templates, `switch`_ to change the
active one, and `unload`_ to drop one.

**Options:**

.. option:: -a RequestReviewID

  Review request ID for the update in automode.
  Can be either in the long (``SUSE:Maintenance:XXXX:YYYYYY`` |
  ``SUSE:SLFO:XXXX:YYYY``) or short
  (``S:M:XXXX:YYYYYY`` | ``S:S:XXXX:YYYY``) format.

.. option:: -k RequestReviewID

  Review request ID for the update in kernel/livepatching mode.
  Can be either in the long (``SUSE:Maintenance:XXXX:YYYYYY`` |
  ``SUSE:SLFO:XXXX:YYYY``) or short
  (``S:M:XXXX:YYYYYY`` | ``S:S:XXXX:YYYY``) format.


list_templates
++++++++++++++

::

    list_templates

Lists all loaded templates. For each template the RRID, connected host count
and workflow mode are shown; the active template is marked with a leading
``*``.


switch
++++++

::

    switch RRID

Makes the given loaded template active. Plain action commands act on the
active template. The RRID must be one of the loaded templates (see
`list_templates`_); an unknown RRID is rejected.


unload
++++++

::

    unload RRID

Unloads one loaded template, closing only its host connections. Other loaded
templates are left untouched. If the unloaded template was the active one, the
next remaining template becomes active.


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

    list_sessions [-t HOST] [-T RRID | --all-templates]

Lists current active ssh sessions on target hosts.

When more than one template is loaded this fans out across all of them by
default (see `Fan-out across templates`_). Use ``-T RRID`` to scope to a
single template.


analyze_diff
++++++++++++

::

    analyze_diff

Analyzes the OBS ``source.diff`` to assist with code review.

The diff is parsed section by section (``changes files:``, ``old:``,
``new:``, ``spec files:`` and the possibly repeating ``other changes:``
blocks), so it copes with complex multi-section diffs that previously
confused the command. It reports:

* source archives changed (``old:`` to ``new:``),
* patches added to and removed from the spec file, flagging any added
  patch that is not mentioned in the changelog,
* version and ``%global``/``%define`` macro bumps,
* bug and CVE references (``bsc#``, ``boo#``, ``CVE-...``) found in the
  changelog.

When patches are applied with ``%autosetup``/``%autopatch`` (or no
explicit ``%patchN`` lines exist), the define-vs-apply cross-check is
skipped instead of emitting a misleading warning.


show_diff
+++++++++

::

    show_diff

Prints source.diff with pager to user.



Internal Commands
*****************

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

config
++++++

::

    config show [option,..] | set option value

Displays or sets runtime MTUI configuration values.

**Options:**

.. option:: show

  Shows config values. ``option`` can be specified.

.. option:: set

  Sets config runtime value ``option`` for ``value``


exit, quit, EOF
+++++++++++++++

::

    exit [reboot|poweroff]
    quit [reboot|poweroff]

Disconnects from all hosts and exits the program.
The tester is asked to save the XML log when exiting MTUI.

``exit`` and ``EOF`` are aliases of ``quit``: ``exit`` is the friendly
synonym, while ``EOF`` is the handler invoked by readline when stdin
reaches end-of-file (typically pressing ``Ctrl-D`` at an empty prompt).
All three accept the same optional ``reboot`` / ``poweroff`` argument.

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

    approve [-h] [-g [GROUP]] [-r REVIEWER]

Wrapper around the `osc qam approve`_ command; approves current update. It is
possible to specify more QA groups for approval.

When ``-r REVIEWER`` is given, the reviewer is recorded in the testreport (the
``Test Plan Reviewer:`` line), the testreport is committed to SVN, and only
then is the update approved. If the testreport has no ``Test Plan Reviewer:``
line or the SVN commit fails, the approval is aborted.

.. _osc qam approve: http://qam.suse.de/projects/oscqam/latest/workflows/tester.html#approve

**Options:**

.. option:: -g [GROUP], --group [GROUP]

  QA group to approve under.

.. option:: -r REVIEWER, --reviewer REVIEWER

  Record REVIEWER in the testreport, commit it to SVN, then approve. Aborts the
  approval if recording the reviewer or the SVN commit fails.


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


comment
+++++++

::

    comment

Adds a comment to the currently loaded review request via OSC. The
command takes no arguments; it prompts interactively for the comment
text on stdin (``Comment:``). The comment is posted against the RRID
of the loaded test report template.


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
