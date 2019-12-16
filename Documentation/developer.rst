#######################
Developer Documentation
#######################

Automated Tests
###############

You can run unit tests with

$ python3 -m pytest 


Commit keywords
###############

Bug ID references
=================

Please refer to relevant bugzilla.suse.com bugs, whenever applicable:

.. code-block:: text

    bsc#<ID>
    boo#<ID>


Documentation
#############

The documentation is generated using `Sphinx`_.

.. _Sphinx: http://sphinx-doc.org/

* Build HTML docs::

    cd Documentation && make html

* Publish the source tarball and HTML docs::

    scp ... root@qam.suse.de:/srv/www/qam.suse.de/media/distfiles
    scp -r ... root@qam.suse.de:/srv/www/qam.suse.de/projects/mtui/$nv
    ssh root@qam.suse.de "cd /srv/www/qam.suse.de/projects/mtui && ln -sf $nv latest"

``$nv`` refers to new version.
