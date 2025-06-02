
########################################################################
                         Command Line Interface
########################################################################

.. contents::

Options
=======

``-a RRID, --auto-review-id RRID``
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

Loads the test report for the maintenance update Review Request ID (RRID) in 
autoinstall update workflow. It skips connecting refhosts and script run in update
command. Downloads logs from openQA.

``RRID`` is a string in the form ``SUSE:Maintenance:XXXX:YYYYYY``, where ``XXXX``
is the Incident ID and ``YYYYYY`` is the Request ID.

``RRID`` can also use the short format ``S:M:XXX:YYYY``.


``-k RRID, --kernel-review-id RRID``
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

Loads the test report for the maintenance update Review Request ID (RRID) in 
kernel update workflow. It skips connecting refhosts and script run in update
command. Downloads logs from openQA.

``RRID`` is a string in the form ``SUSE:Maintenance:XXXX:YYYYYY``, where ``XXXX``
is the Incident ID and ``YYYYYY`` is the Request ID.

``RRID`` can also use the short format ``S:M:XXX:YYYY``.



``-c, --config file``
~~~~~~~~~~~~~~~~~~~~~

Overrides default config files with custom file


``-d, --debug``
~~~~~~~~~~~~~~~

Enables debugging output.

One of the possible use cases for the debug output could be when testing a command
which runs for a long time, as the command output is then printed in real time
instead of after the command has finished.

However, in that case it is more reasonable to use the ``set_log_level`` command
from the interactive user interface (see below).

The default log level is ``info``, while ``-d`` sets it to ``debug``.


``-h, --help``
~~~~~~~~~~~~~~

MTUI will display a short description of its options, operands and their usage,
and exit.


``-l SITE, --location SITE``
~~~~~~~~~~~~~~~~~~~~~~~~~~~~

Overrides the ``mtui.location`` configuration.


``-n, --noninteractive``
~~~~~~~~~~~~~~~~~~~~~~~~

When set, MTUI is run in a non interactive mode without a command shell.
MTUI automatically applies the update and exports the results to the
maintenance template before it quits. User input is not required.


``-p FILE, --prerun FILE``
~~~~~~~~~~~~~~~~~~~~~~~~~~

Runs a script with a set of MTUI commands prior to starting the interactive shell
or the update process. User input is not required if in non interactive mode
(``-n`` parameter).


``-s SPEC, --sut SPEC``
~~~~~~~~~~~~~~~~~~~~~~~

Cumulatively overrides default refhosts given in the test report.

``SPEC`` is a string in the form ``hostname,hostname2..``.


``-t DIR, --template_dir DIR``
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

Overrides the ``mtui.template_dir`` configuration.


``--smelt_api SMELT_API``
~~~~~~~~~~~~~~~~~~~~~~~~~

Overrides the ``mtui.smelt_api`` configuration.

``SMELT_API`` is SMELT2 graphQL endpoint address


``-V, --version``
~~~~~~~~~~~~~~~~~

Prints MTUI version and exits.


``-w SECONDS, --connection_timeout SECONDS``
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

Overrides the ``mtui.connection_timeout`` configuration.
