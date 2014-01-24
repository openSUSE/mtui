#################################
Maintenance Test Update Installer
##################################

MTUI is a tool that allows you to run commands on multiple hosts in
parallel.

License
#######

MTUI is unpublished work of SUSE. You can find the full license at
`LICENSE file<./LICENSE>`_

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
`<https://bugzilla.novell.com/enter_bug.cgi?classification=40&product=Testenvironment&submit=Use+This+Product&component=MTUI>`_

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

Run unit tests with `nosetests tests`

`Acceptance testsuite
<http://git.suse.de/?p=yac/mtui-test-acceptance.git;a=summary>`_ is at
git.suse.de.

Release Engineering
###################

Versioning scheme is based on `SemVer 2.0
<http://semver.org/spec/v2.0.0.html>`_

However, new features introduced in 1.x series are unstable and subject
change and will be stabilized in 2.0 release or on bug report request if
possible.

New commands are masked to the 2.0 release and are not available in 1.x
unless you explicitly ask for them in configuration option
`mtui.command_interface`
