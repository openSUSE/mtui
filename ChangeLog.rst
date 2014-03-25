#########
ChangeLog
#########

1.3.0
#####

backward incompatible
=====================

* Errors on config parsing made more consistent and informative by using
  unified format for config options (<section>.<option>) and including
  the config file path when parsing fails.

  :commits:
    8863337b9b7ab9ec332a618480c059c39a612aa3

new features
============

* config option mtui.chdir_to_templatedir. Applicable only with -m
  argument. See `mtui.cfg.example <./Documentation/mtui.cfg.example>`_
  for details

  :commits:
    b2ac515bfa9c28dd576d43e9ae52d82671d790a8

bugfixes
========

* source_verify with multiple spec files bnc#850727

  :commits:
    0ba8bf4159356005fe00064e4451dba6fcf65937

* minor fixes

  :commits:
    5e114190b8faf73e67f19af696dced239e39f7b5

user experience
===============

* referring the user to BNC#860284 when the error hits.

  :commits:
    3d59271e1a6dcd3e163767399a976386063bf28a

documentation
=============

* Added process description for `submitting code` and `release process`

  :commits:
    72be8fd9bfe2d21e739cf9b0b0437157c0a4826f

internal
========

* cleanup in config

  :commits:
    26710fb1d81e5da1e0720b7b05906ed6a463ea1d
    8863337b9b7ab9ec332a618480c059c39a612aa3

* Getting TIOCGWINSZ from environment variables when ioctl fails to deal
  with tests that require a terminal tty.

  :commits:
    25c0806c90c0d35d203af51ebc66de4fd530a7a2

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
