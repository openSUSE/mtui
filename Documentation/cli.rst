.. vim: tw=72 sts=2 sw=2 et

########################################################################
                         Command Line Interface
########################################################################

.. contents::

Options
=======

-a ATTR, --autoadd=ATTR
~~~~~~~~~~~~~~~~~~~~~~~

autoadd SUT based on attributes

Cumulatively adds refhosts to the target list based on given attributes.

Example::

   mtui -a sles -a 11sp1

-d, --debug
~~~~~~~~~~~

enable debugging output

There might be a use case for debug output when testing a command which
runs for a longer time as the command output is then printed in realtime
instead of after the command has finished. However, it's more reasonable
to use the `set_log_level` command then (see below).

The default loglevel is `INFO` while `-d` sets it to `DEBUG`.

-h, --help
~~~~~~~~~~

MTUI will display short usage description of its options and operands,
and exit.

-l SITE, --location=SITE
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

Overrides the `mtui.location` configuration.

-m HASH, --md5=HASH
~~~~~~~~~~~~~~~~~~~

This parameter simply specifies the MD5 hash for the update which could
be retrieved on the SWAMP QA view. The template path is then composed of
the directory parameter and md5 update hash.
($directory/$md5/log). If the template is not yet checked out
from SVN, MTUI tries to fetch it. When starting MTUI without -m parameter,
a template could be loaded with the load_template command afterwards.

-n, --noninteractive
~~~~~~~~~~~~~~~~~~~~

When set, MTUI is run in an noninteractive mode without a command shell.
MTUI automatically applies the update and exports the results to the
maintenance template before it quits. User input is not required.

-p FILE, --prerun=FILE
~~~~~~~~~~~~~~~~~~~~~~

Runs MTUI commands prior to starting the interactive shell or the update
process. User input is not required if in noninteractive mode (-n parameter).

-r MRID, --review-id=MRID
~~~~~~~~~~~~~~~~~~~~~~~~~

Load testreport maintenance update `MRID`.  `MRID` is a string in the
form `SUSE:Maintenance:X:Y` where `X` is so-called "maintenance id" and
`Y` is "request id".

-s SPEC, --sut SPEC
~~~~~~~~~~~~~~~~~~~

Override refhosts given in the testreport.
`SPEC` is a string in the form `hostname,product`.

-t DIR, --template_dir=DIR
~~~~~~~~~~~~~~~~~~~~~~~~~~

Overrides the `mtui.template_dir` configuration.

-w SECONDS, --connection_timeout=SECONDS
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

Overrides the `mtui.connection_timeout` configuration.