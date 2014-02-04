#########
ChangeLog
#########

next
####

backward incompatible
=====================

* Errors on config parsing made more consistent and informative.
  8863337b9b7ab9ec332a618480c059c39a612aa3

1.2.0
#####

backward incompatible
=====================

* main function wrapper removed.

  * mtui exits with non-zero return code on crash now.

  * no longer hinting which packages are missing as it is distribution
    dependent and unreliable. If you run from packages it's taken care
    of anyway.

  * details at 4c648cfed4374453fd86442ca3d42fb797ac028f

new features
============

* commands: `whoami` and `unlock` under 2.0 API.
  See their help for details.

* config option: mtui.interface_version
  Enables functions of future API version. See
  `docs <./Documentation/mtui.cfg.example>`_ for details.

* env variable MTUI_CONF.
  Path to a config file to read *instead* of the default locations.
  Introduced in order to do automated testing.
  Expected to change to an argv option in the future.

* prompt changed to "mtui > " under 2.0 API.
  see commit 10ae361e78768c1a1465a5cf0aac394f2582ab66 for details.

internal
========

* rewritten locking API
  Localized to mtui.target.Target and deeper as rewriting all the
  depending code in mtui.prompt would be too broad a change.
  Should be sufficiently regtested by new unit tests and acceptance
  testsuite via `set_host_lock` and `list_locks` commands.

* quit command cleanup
  7cc1d677d31c423fea285bfb62fa29438438f622

* introduced mtui.target.HostsGroup as a Composite Pattern to help
  dealing with active hosts selection and interacting with hosts group
  as with single hosts.

* introduced m.com.Command and overrides in m.p.CommandPrompt for better
  command separation and eventualy pluginizing them.

1.1.0
#####

* First release since jmatejka took project maintainership of the
  project after ckornacker

* License changed from GPL to SUSE internal to reflect the current state
  of the project. BNC#850110

* Improved documentation

  * Existing doc was moved under Documentation/

  * README.rst was added as proper doc entry point.

* Improved packaging

  * setup.py switched to setuptools

  * added dependencies

* New features

  * -V argument to print version
