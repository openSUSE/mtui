.. vim: tw=72 sts=2 sw=2 et

########################################################################
                             Configuration
########################################################################

.. contents::

Files
=====

MTUI reads `INI-formatted`_ configuration from two optional files,
``/etc/mtui.cfg`` and ``~/.mtuirc``, with the values found in the latter
overriding those from the former.

Values found in configuration files can be overridden using command line options.

.. _`INI-formatted`: https://docs.python.org/3/library/configparser.html

Directives
==========

This text refers to configuration properties using their section-qualified names.

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


``mtui.datadir``
~~~~~~~~~~~~~~~~

  | **type**
  |     pathname
  | **default**
  |     the MTUI source code directory

MTUI expects testing scripts to be found in this directory.


``mtui.location``
~~~~~~~~~~~~~~~~~

  | **type**
  |     enum: locations defined in `refhosts.yml`_
  | **default**
  |     ``default``

.. _refhosts.yml: https://gitlab.suse.de/qa-maintenance/metadata/blob/master/refhosts.yml

.. tip:: View valid locations using ``refdb``:

    ::

        refdb -p location | sort | uniq

MTUI will limit reference hosts to those found in ``mtui.location``.
If a required system cannot be found in ``mtui.location``, it will be loaded
from ``default``.


``mtui.report_bug_url``
~~~~~~~~~~~~~~~~~~~~~~~

  | **type**
  |     URL
  | **default**
  |     https://bugzilla.suse.com/enter_bug.cgi?classification=40&product=Testenvironment&component=MTUI&submit=Use+This+Product

MTUI bugs are reported via this URL. Used by the `report-bug`_ MTUI command.

.. _report-bug: http://qam.suse.de/projects/mtui/latest/iui.html#report-bug


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


``mtui.install_logs``
~~~~~~~~~~~~~~~~~~~~~

 | **type**
 |     string
 | **default**
 |     install_logs

Name of directory for storing install logs
Please don't change it


``openqa.openqa``
~~~~~~~~~~~~~~~~~

  | **type**
  |     URL
  | **default**
  |     https://openqa.suse.de 

URL of openqa instance


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


``template.smelt_threshold``
~~~~~~~~~~~~~~~~~~~~~~~~~~~~

  | **type**
  |     int 
  | **default**
  |     10 

Set text wrapping for smelt checkers results.
Default is first 10 lines in template.


``testopia.interface``
~~~~~~~~~~~~~~~~~~~~~~

  | **type**
  |     URL
  | **default**
  |     https://apibugzilla.novell.com/tr_xmlrpc.cgi

MTUI accesses Testopia through this URL.


``testopia.pass``
~~~~~~~~~~~~~~~~~

  | **type**
  |     string
  | **default**
  |     <EMPTY>

Password used to log into ``testopia.interface``.
Testopia is integrated with Bugzilla and uses the same credentials.


``testopia.user``
~~~~~~~~~~~~~~~~~

  | **type**
  |     string
  | **default**
  |     <EMPTY>

Username used to log into ``testopia.interface``.
Testopia is integrated with Bugzilla and uses the same credentials.


``url.bugzilla``
~~~~~~~~~~~~~~~~

  | **type**
  |     URL
  | **default**
  |     https://bugzilla.suse.com

Used to construct URLs in Bugzilla- and Testopia-related commands.


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

  [mtui]
  user = <your username>
  location = <your location>
  template_dir = /path/to/where/you/want/to/store/test-reports
  datadir = /usr/share/mtui

  [testopia]
  interface = https://apibugzilla.novell.com/xmlrpc.cgi
  user = <your Bugzilla ID>
  pass = <your Bugzilla password>

  [refhosts]
  resolvers = https
  https_uri = https://qam.suse.de/refhosts/refhosts.yml
  path = /usr/share/qam-metadata/refhosts.yml

  [url]
  bugzilla = https://bugzilla.suse.com

  [openqa]
  openqa = https://openqa.suse.de
