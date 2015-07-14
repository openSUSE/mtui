.. vim: tw=72 sts=2 sw=2 et

########################################################################
                             Configuration
########################################################################

.. contents::

Files
=====

MTUI reads `INI-formatted`_ configuration from two optional files,
`/etc/mtui.cfg` and `~/.mtuirc`, with the values found in the latter
overriding those from the former.  Values found in configuration files
can be overridden using commandline options.

.. _`INI-formatted`: https://docs.python.org/2/library/configparser.html

Directives
==========

This text refers to configuration properties using their section-
qualified names.

mtui.chdir_to_template_dir
~~~~~~~~~~~~~~~~~~~~~~~~~~

type
  enum: `False`, `True`
default
  `False`

If `True`, MTUI will `chdir(2)` into the testreport
checkout directory.

mtui.connection_timeout
~~~~~~~~~~~~~~~~~~~~~~~

type
  seconds
default
  300

Sets the execution timeout to the specified value.
When the timeout limit was hit the user is asked to wait for the current
command to return or to proceed with the next one.
To disable the timeout set it to "0".

mtui.datadir
~~~~~~~~~~~~

type
  pathname
default
  the mtui sourcecode directory

MTUI expects testing scripts in this directory.

mtui.location
~~~~~~~~~~~~~

type
  enum: `beijing`, `cloud`, `consoles`, `default`, `misc`, `nuremberg`, `prague`
default
  `default`

MTUI will limit reference hosts to those in `mtui.location`.
If a required system cannot be found in `mtui.location`
it will be loaded from `default`.

mtui.report_bug_url
~~~~~~~~~~~~~~~~~~~

type
  URL
default
  https://bugzilla.suse.com/enter_bug.cgi?classification=40&product=Testenvironment&component=MTUI&submit=Use+This+Product

Report MTUI bugs at this URL.  Used by the `report-bug` MTUI command.

mtui.tempdir
~~~~~~~~~~~~

type
  pathname
default
  `/tmp`

Temporary local directory for package source checkouts.

mtui.template_dir
~~~~~~~~~~~~~~~~~

type
  pathname
default
  `$TEMPLATE_DIR`, current working directory

All testreports are checked out and stored in this directory.
Specifying the template directory in which the testing directories
are checked out from SVN. If none is given, the current directory
is used. However, this is typically set to another directory
like --template=~/testing/templates. For an improved usability,
the environment variable TEMPLATE_DIR is also processed. Instead of
specifying the directory each time on the commandline, one could set
template_dir="~/testing/templates" in ~/.mtuirc. The commandline
parameter takes precedence over the environment variable if both are given.

mtui.use_keyring
~~~~~~~~~~~~~~~~

type
  enum: `False`, `True`
default
  `False`

If `True`: when `testopia.pass` is non-empty, MTUI will store
its value in the user's keyring; when `testopia.pass` is empty,
MTUI will retrieve it from the user's keyring.

mtui.user
~~~~~~~~~

type
  string
default
  `getpass.getuser()`__

Used in eg. lock files.

.. __: https://docs.python.org/2/library/getpass.html#getpass.getuser


refhosts.https_expiration
~~~~~~~~~~~~~~~~~~~~~~~~~

type
  seconds
default
  43200

Maximum age of the refhost database cache before MTUI will
update it from `refhosts.https_uri` if the `https` resolver is used.

refhosts.https_uri
~~~~~~~~~~~~~~~~~~

type
  URL
default
  https://qam.suse.de/metadata/refhosts.xml

The `https` resolver fetches the refhost database from this URL.

refhosts.path
~~~~~~~~~~~~~

type
  pathname
default
  `/usr/share/suse-qam-metadata/refhosts.xml`

The `path` resolver uses the refhost database at this location.

refhosts.resolvers
~~~~~~~~~~~~~~~~~~

type
  list: {https|path}[,...]
default
  https

This property takes a comma-separated list of resolver types.
Resolvers are tried left-to-right.

svn.path
~~~~~~~~

type
  URL
default
  svn+ssh://svn@qam.suse.de/testreports

MTUI checks out the testreport from, and commits it to,
`${svn.path}/${id}`.

target.repclean
~~~~~~~~~~~~~~~

type
  pathname
default
  `/mounts/qam/rep-clean/rep-clean.sh`

MTUI uses `target.repclean` in refhosts to manipulate package
repositories.  If a refhost does not have `target.repclean`,
MTUI will upload `${mtui.datadir}/helper/rep-clean/rep-clean.{sh,conf}`
to `target.tempdir` and use that copy.

target.tempdir
~~~~~~~~~~~~~~

type
  pathname
default
  `/tmp`

MTUI uploads `rep-clean` files into this directory in refhosts
if needed.

target.testsuitedir
~~~~~~~~~~~~~~~~~~~

type
  pathname
default
  `/usr/share/qa/tools`

MTUI uses testsuites in this directory in refhosts.

testopia.interface
~~~~~~~~~~~~~~~~~~

type
  URL
default
  https://apibugzilla.novell.com/tr_xmlrpc.cgi

MTUI accesses Testopia through this URL.

testopia.pass
~~~~~~~~~~~~~

type
  string
default
  <EMPTY>

Password used to log into `testopia.interface`.
Testopia is integrated with Bugzilla and uses the same credentials.

testopia.user
~~~~~~~~~~~~~

type
  string
default
  <EMPTY>

Username used to log into `testopia.interface`.
Testopia is integrated with Bugzilla and uses the same credentials.

url.bugzilla
~~~~~~~~~~~~

type
  URL
default
  https://bugzilla.novell.com

Used to construct URLs in Bugzilla- and Testopia-related commands.

url.testreports
~~~~~~~~~~~~~~~

type
  URL
default
  http://qam.suse.de/testreports

Prefix to the `Testreport` field value in `list_metadata`
command output.

Example
=======

::

   [mtui]
   template_dir = <where you want to store testreport checkouts>
   location = <your location>

   [testopia]
   user = <your Bugzilla ID>
   pass = <your Bugzilla passwd>
