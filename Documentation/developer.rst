#######################
Developer Documentation
#######################

Submitting code
###############

* Fork the repository.

  * create your personal one on git.suse.de

  * clone stable one

* commit code

* push to your personal repository as a feature/bugfix branch

* send git request-pull

git commit keywords
###################

For referencing Novell Bugzilla bug numbers use format::

    bnc#<N>

where N is the bug number.

API Documentation
#################

For API Documentation use `epydoc <http://epydoc.sourceforge.net/>`_
format with the `rst markup
<http://epydoc.sourceforge.net/manual-fields.html>`_.

Internal rewrite is underway, so pay attention to the `:deprecated:`
markers.

Release Process
###############

* update ChangeLog and mtui.__version__

* git tag v<version>

* python setup.py sdist

* bump ebuild & test

* bump rpm & test

* merge ebuild & rpm changes to stable overlay/repository

* publish tarball on webserver
