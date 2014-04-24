#################################
Maintenance Test Update Installer
#################################

MTUI is a tool that allows you to run commands on multiple hosts in
parallel.

License
#######

MTUI is unpublished work of SUSE. You can find the full license at
`LICENSE file <./LICENSE>`_

Installation
############

openSUSE and SUSE
=================

Packages are available at `IBS home:yac:mtui
<https://build.suse.de/project/show/home:yac:mtui>`_

Gentoo
======

Packages are available at internal `gentoo QAM overlay
<http://git.suse.de/?p=maintenance/gentoo-overlay.git;a=summary>`_

Source
======

Tarballs are available at `deathstar
<http://deathstar.suse.cz/distfiles/>`

Bug reports and feature requests
################################

Can be filed and searched at `Novell Bugzilla
<https://bugzilla.novell.com/enter_bug.cgi?classification=40&product=Testenvironment&submit=Use+This+Product&component=MTUI>`_

Documentation
#############

Documentation is located at ./Documentation where you can find

* `Arguments, MTUI shell commands, configuration and workflow
  <./Documentation/README>`_

* and `FAQ <./Documentation/FAQ>`_

If you want to modify the source code, please take a look at the
`developer documentation <./Documentation/developer.rst>`_


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
`mtui.interface_version config option <./Documentation/mtui.cfg.example>`_
for details.
