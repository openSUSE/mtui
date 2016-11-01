############
Installation
############

.. note::

  After installation, you'll need to configure at least your `location`_
  and you will likely want to customize `template_dir`_ as well.

.. _location: ./cfg.html#mtui-location
.. _template_dir: ./cfg.html#mtui-template-dir

openSUSE and SUSE
#################

.. _IBS QA\:Maintenance project: https://build.suse.de/project/show/QA:Maintenance
.. _project repositories page: https://build.suse.de/project/repositories/QA:Maintenance

Packages are available at `IBS QA:Maintenance project`_.

Add the appropriate repository to your system.

You can find the proper address on the `project repositories page`_ for
each distribution under the "Go to download repository" link.

For example for openSUSE Tumbleweed the command would be:

.. code-block:: sh 

    # zypper ar -f http://download.suse.de/ibs/QA:/Maintenance/openSUSE_Tumbleweed qa-maintenance

Once you have the repository added, you can install the package normally
and get mtui with the appropriate dependencies as well.

.. code-block:: sh 

    # zypper in mtui
