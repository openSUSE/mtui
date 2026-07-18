.. vim: tw=72 sts=2 sw=2 et

########################################################################
                            Configuration
########################################################################

.. contents::

Files
======

MTUI reads `INI-formatted`_ configuration from two optional files,
``/etc/mtui.cfg`` and ``~/.mtuirc``, with the values found in the latter
overriding those from the former.

Values found in configuration files can be overridden using command line options.

.. _`INI-formatted`: https://docs.python.org/3/library/configparser.html

Directives
==========

This text refers to configuration properties using their section-qualified names.

``gitea.token``
~~~~~~~~~~~~~~~
  | **type**
  |     string
  | **default**
  |     `os.getenv('GITEA_TOKEN','')`__

.. __: https://docs.python.org/3/library/os.html#os.getenv

Gitea API access token, this config has higher prio than environment
variable. Token must have full access to the issue API.

``lock.lock.pi_autolock``
~~~~~~~~~~~~~~~~~~~~~~~~~
  | **type**
  |     bool
  | **default**
  |     ``True``

When testing a Product Increment (PI), automatically lock all reference
hosts on ``assign`` with a comment naming the request, and unlock this
session's locks at the end of testing (``unassign`` / ``approve`` /
``reject``). Reference hosts added with ``add_host`` while the
assignment is active are locked too. Set to ``false`` to disable.

``lock.reap_stale``
~~~~~~~~~~~~~~~~~~~
  | **type**
  |     bool
  | **default**
  |     ``True``

When connecting to a reference host, force-remove a pre-existing
``/var/lock/mtui.lock`` that is older than ``lock.stale_age`` regardless
of which user or session created it (including exclusive, commented
locks). Such a lock is almost always left over from a crashed or
abandoned session. Set to ``false`` to only warn about pre-existing
locks, as in older releases. Fresh locks are never removed.

``lock.stale_age``
~~~~~~~~~~~~~~~~~~
  | **type**
  |     int (seconds)
  | **default**
  |     86400

Age, in seconds, beyond which a remote lock is considered stale and
eligible for automatic removal (see ``lock.reap_stale``). A value of
``0`` disables reaping.

``lock.wait``
~~~~~~~~~~~~~
  | **type**
  |     int (seconds)
  | **default**
  |     0

When a reference host is already locked by someone else, queue for it up
to this many seconds instead of failing immediately. While waiting, MTUI
polls the remote lock every ``lock.wait_poll`` seconds until it is freed,
becomes ours, or is reaped as stale; a warning is logged when the wait
starts and again if it times out (after which the usual "locked" error is
raised). A value of ``0`` (the default) preserves the historical
fail-fast behaviour. This is what lets several fanned-out templates queue
politely on an exhausted shared host pool.

``lock.wait_poll``
~~~~~~~~~~~~~~~~~~
  | **type**
  |     int (seconds)
  | **default**
  |     15

Polling interval used while waiting for a busy lock (see ``lock.wait``).
Ignored when ``lock.wait`` is ``0``. Must be a positive integer; zero or
negative values are rejected when the configuration is read and the
default is used instead.

``mtui.chdir_to_template_dir``
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
  | **type**
  |     enum: ``False``, ``True``
  | **default**
  |     ``False``

If set to ``True``, MTUI will ``chdir(2)`` into the test report checkout directory.

.. note::

  **Deprecated**

  Use of the ``mtui.chdir_to_template_dir`` directive is discouraged.


``mtui.connection_timeout``
~~~~~~~~~~~~~~~~~~~~~~~~~~~
  | **type**
  |     seconds
  | **default**
  |     300

Sets the execution timeout to the specified value.

When the timeout limit was hit, the user is asked to wait for the current
command to return or to proceed with the next one.

Must be a positive integer. Zero or negative values are rejected when
the configuration is read — a non-positive timeout would reach the SSH
layer and fail every host connection with a misleading protocol-banner
error — and the default is used instead.


``mtui.install_logs``
~~~~~~~~~~~~~~~~~~~~~
  | **type**
  |     directory name (a single relative path component)
  | **default**
  |     install_logs

Name of directory for storing install logs
Please don't change it

The value is joined per update as ``template_dir/<rrid>/install_logs``,
so it must be a bare directory name: absolute paths and values
containing a path separator are rejected when the configuration is read
and the default is used instead.


``mtui.ssh_strict_host_key_checking``
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
  | **type**
  |     enum: ``auto_add``, ``warn``, ``reject``
  | **default**
  |     ``auto_add``

Selects the paramiko `MissingHostKeyPolicy
<https://docs.paramiko.org/en/stable/api/client.html#paramiko.client.MissingHostKeyPolicy>`_
used when MTUI connects to an SSH host whose key is not yet in the
local ``known_hosts`` file.

``auto_add``
    Silently add the host key to ``known_hosts`` and proceed
    (paramiko's ``AutoAddPolicy``). This is the historical MTUI default
    and is preserved for backward compatibility.

``warn``
    Log a warning and proceed without storing the key
    (``WarningPolicy``). Subsequent connections will warn again.

``reject``
    Refuse the connection (``RejectPolicy``); the SSH operation fails
    with a ``SSHException``.

Unknown values are reported with a warning and fall back to the default
``auto_add`` behaviour.


``mtui.ssl_verify``
~~~~~~~~~~~~~~~~~~~
  | **type**
  |     bool, or path to an existing CA bundle (PEM file or a
        ``c_rehash``-ed certificate directory)
  | **default**
  |     the system's CA bundle when one exists (the interpreter's
        OpenSSL default cafile, honouring ``SSL_CERT_FILE``, e.g.
        ``/etc/ssl/ca-bundle.pem``), otherwise ``True``

Global TLS certificate-verification policy for every outbound HTTP
call (Gitea PR client, QEM Dashboard client, openQA job client, the
openQA / QAM Dashboard search, the log downloads, and the
``refhosts.yml`` fetch). MTUI verifies certificates everywhere out of
the box. When the option is unset — or set to ``true``, which is
deliberately identical — verification prefers the system CA bundle
when one is found: the :mod:`requests` library otherwise validates
only against its bundled ``certifi`` CAs, which do not include
system-installed CAs such as the SUSE root, so internal hosts would
fail verification when MTUI runs from a git checkout even with the CA
properly installed system-wide.

Set ``ssl_verify = false`` to disable verification everywhere, or set
it to a filesystem path (``ssl_verify = /path/to/ca-bundle.pem``) to
verify against a custom CA bundle — a PEM file or a ``c_rehash``-ed
certificate directory (one containing OpenSSL hash-named entries; a
directory of plain ``.pem`` files cannot be used for verification and
is rejected). A leading ``~`` is expanded, a relative path is pinned to
an absolute one at startup (so a later ``chdir_to_template_dir`` cannot
invalidate it), and the path must exist when MTUI starts. A blank value
keeps its historical meaning (verification off) but logs a warning; any
other value is rejected at startup with an error naming the accepted
forms, and MTUI falls back to the (verifying) default.


``mtui.tempdir``
~~~~~~~~~~~~~~~~
  | **type**
  |     pathname
  | **default**
  |     ``$TMPDIR`` | ``/tmp``

Temporary local directory for package source checkouts.


``mtui.template_dir``
~~~~~~~~~~~~~~~~~~~~~
  | **type**
  |     pathname
  | **default**
  |     ``$TEMPLATE_DIR``, current working directory

Specifies the template directory in which the testing directories
are checked out from SVN. If none is given, the current directory
is used. However, this is typically set to another directory such as
``--template=~/testing/templates``.

For an improved usability, the environment variable ``TEMPLATE_DIR`` is also
processed. Instead of specifying the directory each time on the command line,
one could set ``template_dir=~/testing/templates`` in ``~/.mtuirc``.

The command line parameter takes precedence over the environment variable if
both are given.


``mtui.use_keyring``
~~~~~~~~~~~~~~~~~~~~
  | **type**
  |     enum: ``False``, ``True``
  | **default**
  |     ``False``

If set to ``True``: when ``testopia.pass`` is non-empty, MTUI will store
its value in the user's keyring; when ``testopia.pass`` is empty,
MTUI will retrieve it from the user's keyring.


``mtui.user``
~~~~~~~~~~~~~
  | **type**
  |     string
  | **default**
  |     `getpass.getuser()`__

Used e.g. in lock files.

.. __: https://docs.python.org/2/library/getpass.html#getpass.getuser


``mcp.max_output_bytes``
~~~~~~~~~~~~~~~~~~~~~~~~
  | **type**
  |     int (bytes)
  | **default**
  |     100000

Caps the size of a single ``mtui-mcp`` tool result. Output beyond the cap
is truncated with a one-line notice pointing at the ``offset``/``limit``
paging on the testreport read tools. A value of ``0`` disables the cap.
Ignored outside the MCP server. See :doc:`mcp`.


``mcp.session_cap``
~~~~~~~~~~~~~~~~~~~
  | **type**
  |     int
  | **default**
  |     32

Maximum number of concurrent client sessions the ``mtui-mcp`` ``http``
transport will create; a tool call that would exceed the cap fails with a
clear error instead of spawning unbounded SSH connections and worker
threads. Must be a positive integer. Ignored under the ``stdio``
transport. See :doc:`mcp`.


``mcp.session_idle_timeout``
~~~~~~~~~~~~~~~~~~~~~~~~~~~~
  | **type**
  |     int (seconds)
  | **default**
  |     1800

Seconds of inactivity after which an idle ``mtui-mcp`` ``http`` session is
swept and its hosts disconnected. Because the MCP SDK provides no
per-session teardown callback, this sweep is what releases the SSH
connections of a client that simply disconnected. Must be a positive
integer; set to ``0`` to disable reaping. Ignored under the ``stdio``
transport. See :doc:`mcp`.


``mcp.command_pool_size``
~~~~~~~~~~~~~~~~~~~~~~~~~~~
  | **type**
  |     int
  | **default**
  |     ``min(32, cpu + 4)``

Size of the thread pool that runs blocking command bodies under ``mtui-mcp``
(installed as the asyncio event loop's default executor, which
``asyncio.to_thread`` uses). The default matches asyncio's own default pool
size, so ``stdio`` and modest ``http`` deployments are unchanged. When you
raise ``session_cap`` to admit a large agent fleet on the ``http`` transport,
raise this too -- otherwise the number of command bodies executing at once
stays pinned near 32 regardless of how many sessions connect, and calls queue.
Must be a positive integer. See :doc:`mcp`.


``mcp.tool_profile``
~~~~~~~~~~~~~~~~~~~~
  | **type**
  |     enum: ``full``, ``core``
  | **default**
  |     ``full``

Selects which synthesised tools the ``mtui-mcp`` server exposes.
``full`` keeps every command tool. ``core`` exposes only the curated
everyday subset (load/inspect/run/install, report editing, approve/reject,
the ``testreport_*`` and ``job_*`` tools), roughly halving the per-request
tool-list payload the model must carry. Fine-tune either profile with
``mcp.tools_allow`` and ``mcp.tools_deny``. See :doc:`mcp`.


``mcp.tools_allow``
~~~~~~~~~~~~~~~~~~~
  | **type**
  |     list: comma-separated tool names
  | **default**
  |     (empty)

Tool names added back on top of the selected ``mcp.tool_profile``. Applied
before ``mcp.tools_deny``. See :doc:`mcp`.


``mcp.tools_deny``
~~~~~~~~~~~~~~~~~~
  | **type**
  |     list: comma-separated tool names
  | **default**
  |     (empty)

Tool names removed from the exposed set last, after the profile and
``mcp.tools_allow`` have been applied (deny always wins). See :doc:`mcp`.


``obs.api_url``
~~~~~~~~~~~~~~~
  | **type**
  |     URL
  | **default**
  |     ``https://api.suse.de``

OBS/IBS API that mtui acts against for native ``SUSE:Maintenance`` and
Product Increment review actions (``assign``, ``unassign``, ``approve``,
``reject``, ``comment``). No credentials live here: the acting user and
the SSH signing key are read from the user's oscrc, and this value must
match a section header in that oscrc. The oscrc is located exactly like
``osc`` itself — ``$OSC_CONFIG``, then ``$XDG_CONFIG_HOME/osc/oscrc``
(default ``~/.config/osc/oscrc``), then ``~/.oscrc`` — so set
``$OSC_CONFIG`` to point mtui at a non-default oscrc. OBS TLS verification
is governed by ``mtui.ssl_verify``, not by oscrc's own TLS options.

Must be an ``http://`` or ``https://`` URL with a host (an explicit
port must be numeric); an invalid value is rejected when the
configuration is read and the default is used instead.


``obs.request_timeout``
~~~~~~~~~~~~~~~~~~~~~~~
  | **type**
  |     int (seconds)
  | **default**
  |     180

Coarse wall-clock budget checked **between** the individual HTTP calls a
native OBS operation makes; each call is itself bounded by the shared HTTP
timeout. This is not a mid-call hard kill. Must be a positive integer.


``openqa.baremetal``
~~~~~~~~~~~~~~~~~~~~
  | **type**
  |     URL
  | **default**
  |     http://openqa.qam.suse.cz

URL of baremetal openqa instance

Must be an ``http://`` or ``https://`` URL with a host (an explicit
port must be numeric); an invalid value is rejected when the
configuration is read and the default is used instead.


``openqa.distri``
~~~~~~~~~~~~~~~~~
  | **type**
  |     string
  | **default**
  |     sle

Default 'DISTRI' value for openqa jobs


``openqa.install_logfile``
~~~~~~~~~~~~~~~~~~~~~~~~~~
  | **type**
  |     string
  | **default**
  |     update_install-zypper.log 

Name of automatic installation test logfile


``openqa.kernel_install_logfile``
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
  | **type**
  |     string
  | **default**
  |     update_kernel-zypper.log 

Name of kernel installation test logfile


``openqa.openqa``
~~~~~~~~~~~~~~~~~
  | **type**
  |     URL
  | **default**
  |     https://openqa.suse.de 

URL of openqa instance

Must be an ``http://`` or ``https://`` URL with a host (an explicit
port must be numeric); an invalid value is rejected when the
configuration is read and the default is used instead.


``qem_dashboard.api``
~~~~~~~~~~~~~~~~~~~~~
  | **type**
  |     URL
  | **default**
  |     http://dashboard.qam.suse.de/api

URL of the QEM Dashboard API used for incident, aggregate, and auto openQA job discovery.
Kernel workflow still uses openQA directly.

Must be an ``http://`` or ``https://`` URL with a host (an explicit
port must be numeric); an invalid value is rejected when the
configuration is read and the default is used instead.


``refhosts.https_expiration``
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
  | **type**
  |     seconds
  | **default**
  |     43200

Maximum age of the refhost database cache before MTUI will
update it from ``refhosts.https_uri`` if the ``https`` resolver is used.
Must be a positive integer; zero or negative values are rejected when
the configuration is read and the default is used instead.


``refhosts.https_uri``
~~~~~~~~~~~~~~~~~~~~~~
  | **type**
  |     URL
  | **default**
  |     https://qam.suse.de/refhosts/refhosts.yml

The ``https`` resolver fetches the refhost database from this URL.

Must be an ``http://`` or ``https://`` URL with a host (an explicit
port must be numeric); an invalid value is rejected when the
configuration is read and the default is used instead.


``refhosts.path``
~~~~~~~~~~~~~~~~~
  | **type**
  |     pathname
  | **default**
  |     ``/usr/share/qam-metadata/refhosts.yml``

The ``path`` resolver uses the refhost database at this location.


``refhosts.resolvers``
~~~~~~~~~~~~~~~~~~~~~~
  | **type**
  |     list: {https|path}[,...]
  | **default**
  |     https

This property takes a comma-separated list of resolver types.
Resolvers are tried left-to-right.


``teregen.api``
~~~~~~~~~~~~~~~
  | **type**
  |     URL
  | **default**
  |     ``https://qam.suse.de/api/v1``

Base URL of the TeReGen report API, mtui's source of truth for the report data
formerly read from SMELT. It backs the ``checkers`` and ``updates`` commands, the
``regenerate`` command (and the loader's stale-template regeneration offer), and
``assign``'s display of an update's priority and deadline. The locally
checked-out ``metadata.json`` is used as a fallback when the API is unreachable.

Must be an ``http://`` or ``https://`` URL with a host (an explicit
port must be numeric); an invalid value is rejected when the
configuration is read and the default is used instead.


``svn.path``
~~~~~~~~~~~~
  | **type**
  |      URL
  | **default**
  |      svn+ssh://svn@qam.suse.de/testreports

MTUI checks out the testreport from, and commits it to,
``${svn.path}/${id}``.


``target.tempdir``
~~~~~~~~~~~~~~~~~~
  | **type**
  |     pathname
  | **default**
  |     ``/tmp``


``target.testsuitedir``
~~~~~~~~~~~~~~~~~~~~~~~
  | **type**
  |     pathname
  | **default**
  |     ``/usr/share/qa/tools``

MTUI uses testsuites in this directory in refhosts.


``url.bugzilla``
~~~~~~~~~~~~~~~~
  | **type**
  |     URL
  | **default**
  |     https://bugzilla.suse.com

Used to construct URLs in Bugzilla-related commands.


``url.testreports``
~~~~~~~~~~~~~~~~~~~
  | **type**
  |     URL
  | **default**
  |     http://qam.suse.de/testreports

Prefix to the ``Testreport`` field value in ``list_metadata``
command output.

Example
=======

::

  [gitea]
  token = s3cr3t_token
  

  [lock]
  reap_stale = true
  stale_age = 86400
  pi_autolock = true
  wait = 0
  wait_poll = 15
  

  [mtui]
  user = <your username>
  template_dir = /path/to/where/you/want/to/store/test-reports
  

  [obs]
  api_url = https://api.suse.de
  request_timeout = 180
  

  [mcp]
  session_cap = 32
  session_idle_timeout = 1800
  command_pool_size = 36
  tool_profile = full
  max_output_bytes = 100000
  

  [openqa]
  openqa = https://openqa.suse.de
  

  [refhosts]
  resolvers = https
  https_uri = https://qam.suse.de/refhosts/refhosts.yml
  path = /usr/share/qam-metadata/refhosts.yml
  

  [url]
  bugzilla = https://bugzilla.suse.com
  
