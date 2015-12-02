#######################
Developer Documentation
#######################

Automated Tests
###############

You can run unit tests with

.. code-block:: text

   $ make check

or with coverage

.. code-block:: text

   $ make checkcover

And you can find `acceptance test suite`_ at `gitlab.suse.de`_

.. _acceptance test suite: https://gitlab.suse.de/qa-maintenance/mtui-acceptance-tests
.. _gitlab.suse.de: https://gitlab.suse.de

Commit keywords
###############

Bug ID references
=================

Referencing bugzilla.suse.com bugs

.. code-block:: text

    bsc#<ID>

Referencing bugzilla.novell.com (old) bugs

.. code-block:: text

    bnc#<ID>

Documentation
#############

Uses `Sphinx`_.

Build with

.. code-block:: text

    $ cd mtui.git/Documentation
    $ make html

.. _Sphinx: http://sphinx-doc.org/

Release Engineering
###################

Versioning scheme
=================

Versioning scheme is based on `SemVer 2.0`_

.. _SemVer 2.0: http://semver.org/spec/v2.0.0.html

Release Process
===============

.. note::

  Command `isc` refers to `osc` with -A pointing to IBS,
  and `$nv` is the new version.

* branch the official package ::

    isc branch QA:Maintenace mtui

  We'll refer to the destination project with `$bp`.

* update the changelog, `mtui.__version__` in `mtui/__init__.py`,
  and build HTML docs::

    sed -i "/^\(__version__\)\s*=.*/s//\1 = '$nv'/" mtui/__init__.py
    cd Documentation && make html

* if all went well you should commit, tag, and upload the new version
  to IBS::

    git commit mtui/__init__.py -m "Release $nv"
    git tag -a -m "Release $nv" v$nv
    bs-update -P $bp -d . v$nv

* check the build results of the branched package, if there are no
  failures, publish the tag and the package::

    git push origin master v$nv
    isc submitpac $br mtui QA:Maintenance
    isc request accept ...

* bump non-ibs packages manually (see installation) and test them

* publish source tarball and the html docs::

    scp ... root@qam.suse.de:/srv/www/qam.suse.de/media/distfiles
    scp -r ... root@qam.suse.de:/srv/www/qam.suse.de/projects/mtui/$nv
    ssh root@qam.suse.de "cd /srv/www/qam.suse.de/projects/mtui && ln -sf $nv latest"

