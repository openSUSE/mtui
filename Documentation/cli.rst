
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

For SLE16+ and SL-Micro ``RRID`` is string in form ``SUSE:SFLO:XXXX:YYYY`` where ``XXXX``
is branch and ``YYYY`` is pull request number in gitea. Can also use short format 
``S:S:XXXX:YYYY``


``-k RRID, --kernel-review-id RRID``
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

Loads the test report for the maintenance update Review Request ID (RRID) in 
kernel update workflow. It skips connecting refhosts and script run in update
command. Downloads logs from openQA.

``RRID`` is a string in the form ``SUSE:Maintenance:XXXX:YYYYYY``, where ``XXXX``
is the Incident ID and ``YYYYYY`` is the Request ID.

``RRID`` can also use the short format ``S:M:XXX:YYYY``.

For SLE16+ and SL-Micro ``RRID`` is string in form ``SUSE:SFLO:XXXX:YYYY`` where ``XXXX``
is branch and ``YYYY`` is pull request number in gitea. Can also use short format 
``S:S:XXXX:YYYY``


``-c, --config file``
~~~~~~~~~~~~~~~~~~~~~

Overrides default config files with custom file


``--color {auto,always,never}``
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

Controls coloured terminal output.

``auto`` (the default) emits ANSI colour escapes only when stderr is a TTY
and the ``NO_COLOR`` environment variable is unset. ``always`` forces
coloured output regardless of TTY detection — useful when piping into a
pager that understands escapes. ``never`` disables colour entirely.


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


``-s SPEC, --sut SPEC``
~~~~~~~~~~~~~~~~~~~~~~~

Cumulatively overrides default refhosts given in the test report.

``SPEC`` is a string in the form ``hostname,hostname2..``.


``-t DIR, --template_dir DIR``
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

Overrides the ``mtui.template_dir`` configuration.


``-V, --version``
~~~~~~~~~~~~~~~~~

Prints MTUI version and exits.


``-w SECONDS, --connection_timeout SECONDS``
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

Overrides the ``mtui.connection_timeout`` configuration.


``-g string,  --gitea_token string``
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

Overrides the ``gitea.token`` configuration.

``GITEA_TOKEN`` is secret token for gitea api access.
Token must have full access to issue api.
