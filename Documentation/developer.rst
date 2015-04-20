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

* update ``ChangeLog`` and ``mtui.__version__``

* ``git tag v<version>``

* ``python setup.py sdist``

* bump supported packages (see installation) and test them

* build html docs

* merge bumped packages into stable repositories

* publish source tarball and the html docs

* push git tag ``git push <remote> v<version>``
