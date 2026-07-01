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
Ignored when ``lock.wait`` is ``0``.

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

To disable the timeout set it to ``0``.


``mtui.install_logs``
~~~~~~~~~~~~~~~~~~~~~
  | **type**
  |     string
  | **default**
  |     install_logs

Name of directory for storing install logs
Please don't change it


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
  |     bool or string (path)
  | **default**
  |     ``True``

Global TLS certificate-verification policy for every outbound HTTP
call (Gitea PR client, QEM Dashboard client, openQA job client, the
openQA / QAM Dashboard search, the log downloads, and the
``refhosts.yml`` fetch). Defaults to ``True``, so MTUI verifies
certificates everywhere out of the box; reaching internal hosts that
present an internal-CA certificate therefore requires the SUSE CA in
the system trust store. Set ``ssl_verify = false`` to disable
verification everywhere, or set it to a filesystem path
(``ssl_verify = /path/to/ca-bundle.pem``) to verify against a custom CA
bundle.


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


``openqa.baremetal``
~~~~~~~~~~~~~~~~~~~~
  | **type**
  |     URL
  | **default**
  |     http://openqa.qam.suse.cz

URL of baremetal openqa instance


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


``qem_dashboard.api``
~~~~~~~~~~~~~~~~~~~~~
  | **type**
  |     URL
  | **default**
  |     http://dashboard.qam.suse.de/api

URL of the QEM Dashboard API used for incident, aggregate, and auto openQA job discovery.
Kernel workflow still uses openQA directly.


``refhosts.https_expiration``
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
  | **type**
  |     seconds
  | **default**
  |     43200

Maximum age of the refhost database cache before MTUI will
update it from ``refhosts.https_uri`` if the ``https`` resolver is used.


``refhosts.https_uri``
~~~~~~~~~~~~~~~~~~~~~~
  | **type**
  |     URL
  | **default**
  |     https://qam.suse.de/refhosts/refhosts.yml

The ``https`` resolver fetches the refhost database from this URL.


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


``slack.base_url``
~~~~~~~~~~~~~~~~~~~~
  | **type**
  |     URL
  | **default**
  |     https://slack.com/api

Base URL of the Slack Web API used by the ``request_review`` command
and the ``approve`` / ``reject`` review gate. Rarely changed; overridable
mainly so the test suite can point the client at a mock endpoint.


``slack.channel``
~~~~~~~~~~~~~~~~~
  | **type**
  |     string
  | **default**
  |     ``""``

Slack channel the ``request_review`` command posts the review request
to. This must be a channel **ID** (``Cxxxxxxxx``), not a ``#name``; the
bot must be invited to the channel. ``request_review`` errors clearly
when this is unset.


``slack.poll_interval``
~~~~~~~~~~~~~~~~~~~~~~~
  | **type**
  |     int (seconds)
  | **default**
  |     20

Interval, in seconds, at which ``request_review`` polls Slack for thread
replies and the review 👍 reaction while watching a posted request. Keep
this at roughly 20 s or higher: per-template fan-out multiplies the
request rate, and a lower interval risks Slack ``429`` rate limiting.


``slack.token``
~~~~~~~~~~~~~~~
  | **type**
  |     string
  | **default**
  |     `os.getenv('SLACK_TOKEN','')`__

.. __: https://docs.python.org/3/library/os.html#os.getenv

Slack bot token (``xoxb-``) used to post the review request and read
thread replies and reactions. This config has higher priority than the
environment variable. The Slack app needs the ``chat:write``,
``reactions:read``, ``channels:history`` (plus ``groups:history`` for
private channels) and ``users:read`` scopes, and the bot must be invited
to ``slack.channel``.


``slack.watch_timeout``
~~~~~~~~~~~~~~~~~~~~~~~
  | **type**
  |     int (seconds)
  | **default**
  |     28800

Maximum time, in seconds, ``request_review`` waits for a review 👍 before
giving up and returning without approving. A review can take hours, so the
default is a full working day (8h); ``request_review`` blocks with a spinner
in the REPL (Ctrl-C to stop) or runs as a background MCP job for that long.
This also bounds a cancelled MCP background watch job's worker thread, so a
lower value frees a leaked poller sooner.


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
  

  [openqa]
  openqa = https://openqa.suse.de
  

  [refhosts]
  resolvers = https
  https_uri = https://qam.suse.de/refhosts/refhosts.yml
  path = /usr/share/qam-metadata/refhosts.yml


  [slack]
  token = xoxb-your-bot-token
  channel = C0123456789
  poll_interval = 20
  watch_timeout = 28800


  [url]
  bugzilla = https://bugzilla.suse.com
  
