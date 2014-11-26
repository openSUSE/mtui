.. MTUI documentation master file, created by
   sphinx-quickstart on Wed Nov 26 18:12:29 2014.
   You can adapt this file completely to your liking, but it should at least
   contain the root `toctree` directive.

################################
Welcome to MTUI's documentation!
################################

The Maintenance Test Update Installer (MTUI) allows you to run shell
commands on multiple hosts in parallel.

In addition, MTUI provides convenience commands to help with maintenance
update testing and integrating with other systems like bugzilla,
testopia and testreport templates.

License
#######

MTUI is unpublished work of SUSE. You can find the full license at
`LICENSE file <./LICENSE>`_

Bug reports and feature requests
################################

Can be filed and searched at `Novell Bugzilla
<https://bugzilla.suse.com/enter_bug.cgi?classification=40&product=Testenvironment&submit=Use+This+Product&component=MTUI>`_

Documentation
#############

Documentation is located at ./Documentation where you can find

* `Arguments, MTUI shell commands, configuration and workflow
  <./README>`_

* and `FAQ <./FAQ>`_

If you want to modify the source code, please take a look at the
`developer documentation <./developer.rst>`_


Automated Tests
###############

You can run unit tests with

.. sourcecode:: bash

   make check

   # or with coverage:
   make checkcover

And you can find `acceptance test suite`_ at `git.suse.de`_

.. _acceptance test suite: http://git.suse.de/?p=yac/mtui-test-acceptance.git;a=summary
.. _git.suse.de: http://git.suse.de

Release Engineering
###################

Versioning scheme is based on `SemVer 2.0
<http://semver.org/spec/v2.0.0.html>`_

However, new features are introduced under an API mask and subject to
change until stabilized. These need to be explicitly enabled, see
`mtui.interface_version config option <./mtui.cfg.example>`_
for details.

.. toctree::
   :maxdepth: 2

   installation


Indices and tables
==================

* :ref:`genindex`
* :ref:`modindex`
* :ref:`search`

