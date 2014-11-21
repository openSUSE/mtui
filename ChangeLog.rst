#########
ChangeLog
#########

3.0.2
#####

bugfixes
========

* bsc#905964 - command testsuite_submit: on sle12

* bsc#906541 - 'report bug' not working

3.0.1
#####

Bugfixes
========

* Bug 903295 - MTUI hangs on host with non-standard connection port

3.0
###

Bugfixes
========

* bsc#902519 - mtui 3.0.0b2: No such file or directory:
  '/home/<username>/.ssh/config'

* bsc#903255 - Print errors when config parsing errors happen

* bsc#905115 - mtui reports packages as too recent but they aren't (SLE12)

* bsc#903282 - refsearch.py doesn't search for tag 'we'

* bsc#904672 - mtui typo in source_diff build error message

* bsc#904222 - set_location wrong changing output

* bsc#904701 - MTUI list_downgrade_commands missing and help option linked
  to "list_update_commands" (edit)

* bsc#904224 - mtui set_location accepts invalid location

* bsc#902689 - Traceback returned when incorrect parameters provided with
  list_packages

* bsc#904381 - mtui continues even when svn repo is not "accessible"

New features
============

* bsc#860234 - New command: report-bug to open web browser pointed to
  mtui bugzilla with fields common for all mtui bugs prefilled

* Commands unlock and config stabilized since the 3.0 version

Internal
========

* More improvements to compatibility with python 3

3.0.0b2
#######

Bugfixes
========

* Fix SLE12 updater to code so it works with multiple addon/module
  repositories

3.0.0b1
#######

Bugfixes
========

* bnc#885898 - mtui consumes a lot resources on kernel updates

* bnc#888204 - Traceback returned when incorrect password provided when
    using interface_version=3.0

* bnc#889566 - command source_verify: make nicer output for multiple
    spec files. Makes the output easier to read and the command itself
    reliable in case there is multiple spec files and some of them have
    no patches.

* Install and uninstall commands works without testreport loaded.
    However it will still break if you are connected to hosts that
    require different installation commands.

New features
============

* SLE 12 critical features support. Such as load_template,
  list_packages, source_extract, source_verify, install, uninstall,
    update, downgrade and export.

* New config option mtui.use_keyring so using keyring can be disabled
    for mtui if the keyring module is present on the system.

* ${HOME}/.ssh/config is respected when connecting to hosts.
    Thanks to Roman Neuhauser for this feature.

* Colors can now be disabled by exporting COLOR=never into environment.

* command list_packages can be given -p argument to specify package to
    list.

* A special case to attributes handling was added so ``sle`` is
    recognized as either ``sles`` or ``sled`` so user can ask for ``sle
    12`` and will be connected to SLE 12 machines without OpenSUSE 12.

Internal
========

* Lots of improvements to be more comaptible to python 3 thanks to
    Roman Neuhauser.

* Lots of other refactorings.

Backward incompatible
=====================

* command list_testsuite_commands was removed.

* command list_packages changed arguments.
    ``list_packages all`` is now just ``list_packages``.
    ``list_packages`` is now ``list_packages -w``

* command source_install was removed since it was broken since Nov 2012
  anyway.

2.0.0
#####

Bugfixes
========

* Fix bnc#870198 - host parsing in "unlock" command

  :commits:
    a753d5c2409d82b13d1954dde4947b11acfec41c


* Proper implementation for prerun

  :commits:
    3390bcf517f875809869679784da4f978cec8ec5

  The cmd.Cmd has been deduplicated and prerun now supports
  class-defined commands

new features
============

* bnc#850119 Separate refhosts

  :commits:
    d859329beb0d15dd45d0e70fc552c851557eab68

  Configuration changes:

    * mtui.refhosts_xml changed to refhosts.refhosts_path and is
      applicable only if refhosts.resolvers includes "path" resolver.

    * refhosts.resolvers is treated as comma separated list of resolvers
      (path or https).

    * for https resolver, additional config refhosts.https_uri and
      refhosts.expiration are available and defaults to our qam refhosts
      uri and 12 hours, respectively.

* After testreport template is parsed, it is reported (warning) which
  parameters were not found.

  :commits:
    c5be08045be67574619b7dc09c0f943d888f3388

backwards incompatible improvements
===================================

* New commands not ready for stabilization were bumped to 3.0
  Meaning: if you were using interface_version=2.0 you will need to
  reconfigure to 3.0

* Cleaned up arguments parsing & naming to better convey the meaning of
  what they do and change some to take saner format

  :commits:
    c48717289421f3f176b8e2f18918d29f958b7698

  * Argument changes:

      * timeout      -> connection_timeout

      * search-hosts -> cumulative autoadd

      * overwrite    -> cumulative sut

      * verbose      -> debug

  * Unify naming between config options and CLI arguments

    * template dir:
        argv:   --templates      -> --template_dir
        config: mtui.templatedir -> mtui.template_dir
        env:    TEMPLATEDIR      -> TEMPLATE_DIR

        and consequently config option
        mtui.chdir_to_templatedir -> mtui.chdir_to_template_dir

    * timeout:
        config: connection.timeout -> mtui.connection_timeout
        argv:   --timeout -> --connection_timeout

  * Arguments location, connection_timeout and template_dir are now config
    overrides (this is probably rather internal only change)

  * Remove option dryrun as theoretically unsound and not well defined

  * Switch from getopt to argparse which results in

      * automatic non-zero exit code (bugfix)

      * better parse failure messages (UX)

      * and simpler parser maintenance (internal)

      * fixed out of sync usage - --templates option
        since ea2e9abd9bbdedc8b6002c49c60d44c6c7a5e19b

  * properly parsed md5 so it doesn't accept strings longer than 32
    chars

  * Dead code removal - check_modules() should have been removed as part
    of commit 4c648cfed4374453fd86442ca3d42fb797ac028f

* `config` command changed to `config show` with additional arguments

* prompt changed to "mtui> "

  :commits:
    d4cdd93657a8637e8a10690788b57f8349f4b377

    To be more consistent with other tools (eg. gpg) and more esthetically
    pleasing

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
