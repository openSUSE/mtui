# -*- coding: utf-8 -*-
# vim: et sw=2 sts=2

from __future__ import print_function
from __future__ import absolute_import

from nose.tools import eq_, ok_

class Results(object):
  def __init__(self):
    self.bugs = dict()
    self.patches = dict()
    self.systems = dict()
    self.testplatforms = list()
  def __setattr__(self, name, val):
    self.__dict__[name] = val

import mtui.parsemeta as parsemeta

class MetadataParserTest(object):
  def __init__(t, P, fixture):
    t.make_parser = P
    t.data = fixture + [
      ('Category: omg fubar', 'category', 'omg fubar'),
      ('Packager: rofl lmao', 'packager', 'rofl lmao'),
      ('Packages: wpa_supplicant >= 2.2-8.1, wpa_supplicant-gui >= 2.2-8.1',
        'packages',
        {
          'wpa_supplicant': '2.2-8.1',
          'wpa_supplicant-gui': '2.2-8.1',
        },
      ),
      ('Suggested Test Plan Reviewer: snafubar roflmao',
        'reviewer', 'snafubar roflmao'),
      ('Test Plan Reviewers: snafu fubar',
        'reviewer', 'snafu fubar'),
      ('Testplatform: base=sled(major=12,minor=);arch=[x86_64]',
        'testplatforms', ['base=sled(major=12,minor=);arch=[x86_64]']),
      ('Bugs: 900611, 915323, 927558', 'bugs',
        {
          '900611': 'Description not available',
          '915323': 'Description not available',
          '927558': 'Description not available',
        },
      ),
      ('Repository: something something', 'repository', 'something something'),
      ('sled12None-x86_64 (reference host: sna.fub.ar)', 'systems',
        {
          'sna.fub.ar': 'sled12None-x86_64'
        },
      ),
    ]

  def test(t):
    for inp, att, exp in t.data:
      yield t.doit, inp, att, exp

  def doit(t, inp, att, exp):
    p = t.make_parser()
    r = Results()
    ok_(p.parse_line(r, inp), 'input rejected')
    eq_(getattr(r, att), exp)


class TestOBSMetadataParser(MetadataParserTest):
  def __init__(t):
    from mtui.types.obs import RequestReviewID

    super(TestOBSMetadataParser, t).__init__(
      parsemeta.OBSMetadataParser,
      [
        ('Rating: da best evah', 'rating', 'da best evah'),
        ('ReviewRequestID: SUSE:Maintenance:42:69', 'rrid', RequestReviewID('SUSE:Maintenance:42:69')),
      ],
    )


class TestSWAMPMetadataParser(MetadataParserTest):
  def __init__(t):
    from mtui.types import MD5Hash

    super(TestSWAMPMetadataParser, t).__init__(
      parsemeta.SWAMPMetadataParser,
      [
        ('MD5 sum: DEADBEEF', 'md5', MD5Hash('DEADBEEF')),
        ('SUBSWAMPID: 9624', 'swampid', '9624'),
        ('RES Patch No: 4269', 'patches', dict(res = '4269')),
        ('SAT Patch No: 6942', 'patches', dict(sat = '6942')),
        ('YOU Patch No: 6429', 'patches', dict(you = '6429')),
        ('ZYPP Patch No: 4692', 'patches', dict(zypp = '4692')),
      ],
    )
