# -*- coding: utf-8 -*-

# copy&pasted from (MIT-licensed) six.py at
# https://bitbucket.org/gutworth/six/src/8e634686c53a35/six.py?at=default
#
# this cannot be in mtui/utils.py because of a cyclic dependency
# between that file and mtui/messages.py
def with_metaclass(meta, *bases):
    """Create a base class with a metaclass."""
    # This requires a bit of explanation: the basic idea is to make a dummy
    # metaclass for one level of class instantiation that replaces itself with
    # the actual metaclass.
    class metaclass(meta):
        def __new__(cls, name, this_bases, d):
            return meta(name, bases, d)
    return type.__new__(metaclass, 'temporary_class', (), {})

