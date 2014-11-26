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

And you can find `acceptance test suite`_ at `git.suse.de`_

.. _acceptance test suite: http://git.suse.de/?p=yac/mtui-test-acceptance.git;a=summary
.. _git.suse.de: http://git.suse.de

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

* update ChangeLog and mtui.__version__

* git tag v<version>

* python setup.py sdist

* bump supported packages (see installation) and test them

* merge bumped packages into stable repositories

* publish source tarball and push git tag
