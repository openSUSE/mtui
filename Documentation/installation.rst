############
Installation
############

openSUSE and SUSE
#################

.. _IBS QA\:Maintenance project: https://build.suse.de/project/show/QA:Maintenance
.. _project repositories page: https://build.suse.de/project/repositories/QA:Maintenance

Packages are available at `IBS QA:Maintenance project`_.

Add the appropriate repository to your system. You can find the proper
address on the `project repositories page`_ for each distribution under
the "Go to download repository" link.

For example, on openSUSE Tumbleweed:

.. code-block:: sh

    # zypper ar -f http://download.suse.de/ibs/QA:/Maintenance/openSUSE_Tumbleweed qa-maintenance
    # zypper in mtui

This pulls in mtui together with all required dependencies.

.. note::

  After installation, you'll need to configure at least your `location`_
  and you will likely want to customize `template_dir`_ as well.

.. _location: ./cfg.html#mtui-location
.. _template_dir: ./cfg.html#mtui-template-dir


From source
###########

mtui is not published on PyPI. To install from a checkout you can use
either ``uv`` (recommended) or ``pip``.

With uv
=======

`uv <https://docs.astral.sh/uv/>`_ manages a project-local virtualenv
and pins dependencies via ``uv.lock``.

.. code-block:: sh

    git clone https://github.com/openSUSE/mtui.git
    cd mtui
    uv sync --extra norpm --group dev
    uv run mtui --help

With pip
========

.. code-block:: sh

    git clone https://github.com/openSUSE/mtui.git
    cd mtui
    pip install -e '.[norpm]'
    mtui --help

Optional extras
===============

The following extras can be combined (for example
``pip install -e '.[norpm,keyring,notify,completion]'``):

``norpm``
    Pulls in `version_utils <https://pypi.org/project/version-utils/>`_
    so the rpm version-parsing fallback works without the system
    ``rpm`` Python bindings. Recommended for non-SUSE systems.

``rpm``
    Use the system ``rpm`` Python bindings instead of ``version_utils``.
    Only available where the bindings are installable from PyPI, which
    in practice means SUSE systems with the ``rpm-python`` package.

``keyring``
    Adds `keyring <https://pypi.org/project/keyring/>`_ support for
    storing credentials in the OS keyring instead of plain text.

``notify``
    Adds `notify <https://pypi.org/project/notify/>`_ for desktop
    notifications.

``completion``
    Adds `argcomplete <https://pypi.org/project/argcomplete/>`_ to
    enable shell-completion for ``mtui``. After installing, activate it
    in your shell rc file with::

        eval "$(register-python-argcomplete mtui)"

    Bash users additionally need the ``bash-completion`` package
    installed.

System-package requirements
===========================

mtui invokes a small number of external CLI tools as subprocesses; these
must be installed separately from the Python dependencies.

``osc``
    The OBS / IBS command-line client. mtui shells out to ``osc`` for
    every action against ``SUSE:Maintenance`` requests; the connector
    does not import ``osc`` as a library. On openSUSE this is the ``osc``
    package; on other distributions install it from the
    `openSUSE:Tools <https://build.opensuse.org/project/show/openSUSE:Tools>`_
    project or via ``pipx install osc``.

``ssh`` / ``scp``
    Required by paramiko's underlying configuration parsing and by
    several mtui helper scripts. Any modern OpenSSH client suffices.
