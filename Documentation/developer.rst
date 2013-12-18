#######################
Developer Documentation
#######################

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

Testing
#######

Besides the unit tests included in this repository, there is an
acceptance `test suite
<http://git.suse.de/?p=yac/mtui-test-acceptance.git;a=summary>`_. Feel
free to clone it and send pull requests but don't push there.
