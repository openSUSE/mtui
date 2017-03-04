# -*- coding: utf-8 -*-

from nose.tools import ok_, eq_
from nose.tools import raises

import os
from tempfile import NamedTemporaryFile
from posix import stat_result
from errno import EPERM
from urllib.error import URLError

from mtui import messages
from mtui.refhost import Attributes
from mtui.refhost import _RefhostsFactory
from mtui.refhost import RefhostsFactory
from mtui.refhost import RefhostsResolveFailed
from mtui.refhost import Refhosts
from .utils import LogFake
from .utils import LogTestingWrap
from .utils import RefhostsFake
from .utils import get_nonexistent_path
from .utils import ConstMtimeStat
from .utils import ConstFloat
from .utils import Raiser
from .utils import unused
from .utils import ConfigFake
from .utils import CallLogger
from .utils import StringIO
from .utils import refhosts_fixtures
from .utils import random_alphanum

def test_factory_instance():
    ok_(isinstance(RefhostsFactory, _RefhostsFactory))

# {{{ test _RefhostsFactory __call__ and resolve_X
def test_rf_call_https():
    """
    Test L{_RefhostsFactory.__call__} https resolver
    """
    f = _RefhostsFactory(
      unused
    , os.stat
    , lambda _: StringIO("fooxml")
    , lambda *_: None
    , NamedTemporaryFile().name
    , RefhostsFake
    )
    f.resolve_path = CallLogger()
    c = ConfigFake(overrides = dict(refhosts_resolvers = 'https'))
    r = f(c, LogFake())

    eq_(len(f.resolve_path.calls), 0)
    ok_(isinstance(r, f.refhosts_factory))

def test_rf_call_path():
    """
    Test L{_RefhostsFactory.__call__} path resolver
    """
    f = _RefhostsFactory(
      unused
    , os.stat
    , lambda _: StringIO("fooxml")
    , lambda *_: None
    , NamedTemporaryFile().name
    , RefhostsFake
    )
    f.resolve_https = CallLogger()
    c = ConfigFake(overrides = dict(refhosts_resolvers = 'path'))
    r = f(c, LogFake())

    eq_(len(f.resolve_https.calls), 0)
    ok_(isinstance(r, f.refhosts_factory))

def test_rf_call_both_first_success():
    """
    Test L{_RefhostsFactory.__call__} https,path resolvers. First success
    """
    f = _RefhostsFactory(
      unused
    , os.stat
    , lambda _: StringIO("fooxml")
    , lambda *_: None
    , NamedTemporaryFile().name
    , RefhostsFake
    )
    f.resolve_path = CallLogger()
    c = ConfigFake(overrides = dict(refhosts_resolvers = 'https,path'))
    r = f(c, LogFake())

    eq_(len(f.resolve_path.calls), 0)
    ok_(isinstance(r, f.refhosts_factory))

def test_rf_call_both_second_success():
    """
    Test L{_RefhostsFactory.__call__} https,path resolvers. Second success
    """
    f = _RefhostsFactory(
      unused
    , unused
    , Raiser(URLError('Name or service not known'))
    , unused
    , unused
    , RefhostsFake
    )
    f._is_https_cache_refresh_needed = lambda *_: True
    c = ConfigFake(overrides = dict(refhosts_resolvers = 'https,path'))
    l = LogFake()
    r = f(c, l)

    eq_(l.warnings, ['Refhosts: resolver https failed'])
    ok_(isinstance(r, f.refhosts_factory))

def test_rf_call_no_resolvers():
    """
    Test L{_RefhostsFactory.__call__} https,path resolvers. No success
    """
    f = _RefhostsFactory(
      unused
    , unused
    , Raiser(URLError('Name or service not known'))
    , unused
    , unused
    , RefhostsFake
    )
    f._is_https_cache_refresh_needed = lambda *_: True
    f.resolve_path = Raiser(IOError())
    c = ConfigFake(overrides = dict(refhosts_resolvers = 'https,path'))
    l = LogFake()
    try:
        r = f(c, l)
    except RefhostsResolveFailed:
        pass

    eq_(l.warnings, [
          'Refhosts: resolver https failed'
        , 'Refhosts: resolver path failed'
    ])

def test_rf_call_invalid_resolver():
    """
    Test L{_RefhostsFactory.__call__} invalid resolver
    """
    f = _RefhostsFactory(
      unused
    , unused
    , unused
    , unused
    , unused
    , RefhostsFake
    )
    c = ConfigFake(overrides = dict(refhosts_resolvers = 'http,path'))
    l = LogFake()
    r = f(c, l)

    ok_(isinstance(r, f.refhosts_factory))
    eq_(l.warnings, [
          'Refhosts: invalid resolver: http'
        , 'Refhosts: resolver http failed'
    ])
# }}}

# {{{ Test resolvers

def test_rf_rh():
    """
    Test L{_RefhostsFactory.resolve_https} calls and returns Refhosts
    """
    tmp = NamedTemporaryFile()
    tmp.write(
        b'<?xml version="1.0" encoding="utf-8"?>' \
        + b'<definitions></definitions>'
    )
    tmp.flush()

    f = _RefhostsFactory(unused, unused, unused, unused, tmp.name)
    f.refresh_https_cache_if_needed = CallLogger()
    c = ConfigFake(overrides = dict(location = 'foolocation'))
    l = LogTestingWrap()
    r = f.resolve_https(c, l.log)
    ok_(isinstance(r, f.refhosts_factory))
    eq_(r.location, c.location)
    eq_(len(f.refresh_https_cache_if_needed.calls), 1)
    eq_(l.all(), LogTestingWrap.empty())

def test_rf_rp():
    """
    Test L{_RefhostsFactory.resolve_path} calls and returns Refhosts
    """
    f = _RefhostsFactory(
      unused
    , unused
    , unused
    , unused
    , unused
    , Refhosts
    )
    c = ConfigFake(overrides = dict(
        refhosts_path = refhosts_fixtures['basic']
        , location = 'foolocation'))
    l = LogTestingWrap()
    r = f.resolve_path(c, l.log)
    ok_(isinstance(r, f.refhosts_factory))
    eq_(r.location, c.location)
    eq_(len(r._location_hosts(c.location)), 3)
    eq_(l.all(), LogTestingWrap.empty())
# }}}

# {{{
def test_rf_ihcrn_cache_missing():
    """
    Test L{_RefhostsFactory._is_https_cache_refresh_needed}: cache file missing
    """
    f = RefhostsFactory
    ok_(f._is_https_cache_refresh_needed(get_nonexistent_path(), unused))

def test_rf_ihcrn_not_needed():
    """
    Test L{_RefhostsFactory._is_https_cache_refresh_needed}: refresh is not needed
    """
    f = _RefhostsFactory(ConstFloat(7), ConstMtimeStat(5)
    , unused, unused, unused)
    ok_(not f._is_https_cache_refresh_needed(unused, 2))

def test_rf_ihcrn_is_needed():
    """
    Test L{_RefhostsFactory._is_https_cache_refresh_needed}: refresh is needed
    """
    f = _RefhostsFactory(ConstFloat(7), ConstMtimeStat(5)
    , unused, unused, unused)
    ok_(f._is_https_cache_refresh_needed(unused, 1))

@raises(OSError)
def test_rf_ihcrn_os_error():
    """
    Test L{_RefhostsFactory._is_https_cache_refresh_needed}: stat raises
    """
    f = _RefhostsFactory(unused, Raiser(OSError(EPERM))
    , unused, unused, unused)
    ok_(f._is_https_cache_refresh_needed(unused, unused))
# }}}

# {{{
def test_rf_rhcin_calls():
    """
    Test L{_RefhostsFactory.refresh_https_if_needed} calls refresh
    """
    c = ConfigFake()
    f = _RefhostsFactory(*[unused for _ in range(5)])
    f._is_https_cache_refresh_needed = lambda *_, **__: True
    f.refresh_https_cache = CallLogger()

    cache_file = "foobar"
    f.refresh_https_cache_if_needed(cache_file, c)

    eq_(len(f.refresh_https_cache.calls), 1)
    #eq_(f.refresh_https_cache.calls[0], ((cache_file,
    #    config.refhosts_https_expiration), {}))

def test_rf_rhcin_not_calls():
    """
    Test L{_RefhostsFactory.refresh_https_if_needed} not calls refresh
    """
    f = _RefhostsFactory(*[unused for _ in range(5)])
    f._is_https_cache_refresh_needed = lambda *_, **__: False
    f.refresh_https_cache = CallLogger()

    f.refresh_https_cache_if_needed(unused, ConfigFake())

    eq_(len(f.refresh_https_cache.calls), 0)
# }}}

# {{{
def test_rf_rhc():
    """
    Test L{_RefhostsFactory.refresh_https_cache} calls
    """
    f = _RefhostsFactory(unused, unused, lambda _: StringIO("fooxml"),
        CallLogger(), unused)
    f.refresh_https_cache('foopath', 'foouri')
    eq_(len(f._write_file.calls), 1)
    eq_(f._write_file.calls[0][0][0], "fooxml")
# }}}

def check_search(loc, fixture, expect):
    l = LogTestingWrap()
    r = Refhosts(
        refhosts_fixtures[fixture]
      , l.log
      , loc
    )
    a = Attributes()
    a.major = '11'
    a.minor = 'sp3'
    eq_(r.search(a), expect)
    eq_(l.all(), LogTestingWrap().all())

def test_search_with_mutliple_locations():
    check_search(
        "foolocation"
      , 'multiple-locations'
      , ["cunningham.example.com", "fletcher.example.com"]
    )

def test_search_no_fallback():
    """
    Test fallback hosts are not returned when all needed hosts were
    found in the requested location
    """
    check_search(
        "foolocation"
      , "basic"
      , [
            "cunningham.example.com"
          , "rivers.example.com"
          , "fletcher.example.com"
    ])

def test_check_location_sanity():
    """
    Test Refhosts.check_location_sanity returns for valid locations
    and raises for invalid ones
    """
    locs = [random_alphanum(1, 10) for _ in range(0, 5)]
    while True:
        invalid = random_alphanum(1, 10)
        if invalid not in locs:
            break

    tmp = NamedTemporaryFile()
    tmp.write( bytes(
        '<?xml version="1.0" encoding="utf-8"?><definitions>'
      + ''.join(['<location name="{}"></location>'.format(x)
            for x in locs])
      + '</definitions>',
        'utf-8'
    ))
    tmp.flush()

    l = LogTestingWrap()
    r = Refhosts(tmp.name, l.log)
    for loc in locs:
        r.check_location_sanity(loc)

    try:
        r.check_location_sanity(invalid)
    except messages.InvalidLocationError:
        pass
    else:
        ok_(False, "Expected messages.InvalidLocationError")

# {{{ dependency checks
def test_rf_stat():
    """
    Test L{RefhostsFactory._stat} returns L{stat_result}
    """
    tmp = NamedTemporaryFile()
    ok_(isinstance(RefhostsFactory._stat(tmp.name), stat_result))

def test_rf_time_now():
    """
    Test L{RefhostsFactory.get_unix_time_now} returns sanely large float
    """
    t = RefhostsFactory._time_now()
    ok_(isinstance(t, float))
    ok_(t > 1404839444)
# }}}
