############
Installation
############

openSUSE and SUSE
#################

.. _IBS QA\:Maintenance project: https://build.suse.de/project/show/QA:Maintenance
.. _project repositories page: https://build.suse.de/project/repositories/QA:Maintenance

Packages are available at `IBS QA:Maintenance project`_.

Add the appropriate repository to your system.

You can find the proper address on the `project repositories page`_ for
each distribution under the "Go to download repository" link.

For example for openSUSE Factory the command would be:

.. code-block:: text

    # zypper ar -f http://download.suse.de/ibs/QA:/Maintenance/openSUSE_Factory qa-maintenance

Once you have the repository added, you can install the package normally
and get mtui with the appropriate dependencies as well.

.. code-block:: text

    # zypper in mtui

Gentoo
######

Packages are available at internal `gentoo QAM overlay`_

.. _gentoo QAM overlay: http://git.suse.de/?p=maintenance/gentoo-overlay.git

Source
######

Tarballs are available at `deathstar.suse.cz`_

.. _deathstar.suse.cz: http://deathstar.suse.cz/distfiles/
