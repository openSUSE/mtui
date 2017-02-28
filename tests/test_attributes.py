# -*- coding: utf-8 -*-
# vim: et sw=2 sts=2




from nose.tools import ok_, eq_
from nose.tools import raises

import tests.utils as tut

from mtui.refhost import Attributes

def make_logger():
  return tut.LogFake()


def test_construction():
  a = Attributes()
  eq_(a.product, '')
  eq_(a.major, None)
  eq_(a.minor, None)
  eq_(a.release, None)
  eq_(a.kernel, None)
  eq_(a.ltss, None)
  eq_(a.minimal, None)

  eq_(a.archs, list())
  eq_(a.addons, dict())
  eq_(a.virtual, dict(mode = '', hypervisor = ''))

  eq_(str(a), '')

  eq_(bool(a), False)


class Test_stringification(object):
  def make_attrs(t):
    return Attributes()

  def testcase_1(ctx):
    a = ctx.make_attrs()

    a.product = 'sturd'
    a.major = '42'
    a.minor = '69'
    a.release = 'fefe'
    a.archs = 'Bfoo Abar Cqux'.split()
    eq_(str(a), 'sturd 42.69fefe Abar Bfoo Cqux')

    a.kernel = True
    a.ltss = True
    a.minimal = True
    eq_(str(a), 'sturd 42.69fefe Abar Bfoo Cqux kernel ltss minimal')

    a.virtual['mode'] = 'guest'
    eq_(str(a), 'sturd 42.69fefe Abar Bfoo Cqux kernel ltss minimal guest')

    a.virtual['hypervisor'] = 'rofl.pam.suici.de'
    eq_(str(a), 'sturd 42.69fefe Abar Bfoo Cqux kernel ltss minimal guest rofl.pam.suici.de')

    a.kernel = False
    eq_(str(a), 'sturd 42.69fefe Abar Bfoo Cqux ltss minimal guest rofl.pam.suici.de')

    a.ltss = False
    eq_(str(a), 'sturd 42.69fefe Abar Bfoo Cqux minimal guest rofl.pam.suici.de')

  def testcase_2(ctx):
    a = ctx.make_attrs()

    a.addons['foo'] = dict(major = '12', minor = '34')
    a.addons['qux'] = dict(major = '21')
    a.addons['bar'] = dict(major = '45', minor = '67')
    a.addons['zal'] = dict(minor = '43')

    eq_(str(a), 'bar 45.67 foo 12.34 qux 21. zal .43')


class Test_from_testplatform(object):
  def make_attrs(ctx, tp):
    return Attributes.from_testplatform(tp, make_logger())

  def testcase_2(ctx):
    tp = 'base=sap-aio(major=12,minor=);arch=[x86_64]'
    a = ctx.make_attrs(tp)
    eq_(a.product, 'sap-aio')
    eq_(a.major, '12')
    eq_(a.minor, '')
    eq_(a.archs, ['x86_64'])
    eq_(a.addons, dict())
    eq_(a.virtual, dict(hypervisor = '', mode = ''))
    eq_(str(a), 'sap-aio 12 x86_64')

  def testcase_3(ctx):
    tp = 'base=sled(major=11,minor=sp3);arch=[i386,x86_64]'
    a = ctx.make_attrs(tp)
    eq_(a.product, 'sled')
    eq_(a.major, '11')
    eq_(a.minor, 'sp3')
    eq_(a.archs, ['i386', 'x86_64'])
    eq_(a.addons, dict())
    eq_(a.virtual, dict(hypervisor = '', mode = ''))
    eq_(str(a), 'sled 11sp3 i386 x86_64')

  def testcase_3(ctx):
    tp = 'base=sled(major=12,minor=);arch=[x86_64]'
    a = ctx.make_attrs(tp)
    eq_(a.product, 'sled')
    eq_(a.major, '12')
    eq_(a.minor, '')
    eq_(a.archs, ['x86_64'])
    eq_(a.addons, dict())
    eq_(a.virtual, dict(hypervisor = '', mode = ''))
    eq_(str(a), 'sled 12 x86_64')

  def testcase_4(ctx):
    tp = 'base=sled(major=12,minor=);arch=[x86_64];virtual=(hypervisor=kvm)'
    a = ctx.make_attrs(tp)
    eq_(a.product, 'sled')
    eq_(a.major, '12')
    eq_(a.minor, '')
    eq_(a.archs, ['x86_64'])
    eq_(a.addons, dict())
    eq_(a.virtual, dict(hypervisor = 'kvm', mode = ''))
    eq_(str(a), 'sled 12 x86_64 kvm')

  def testcase_5(ctx):
    tp = 'base=sles(major=11,minor=sp3);arch=[i386,s390x,x86_64]'
    a = ctx.make_attrs(tp)
    eq_(a.product, 'sles')
    eq_(a.major, '11')
    eq_(a.minor, 'sp3')
    eq_(a.archs, ['i386', 's390x', 'x86_64'])
    eq_(a.addons, dict())
    eq_(a.virtual, dict(hypervisor = '', mode = ''))
    eq_(str(a), 'sles 11sp3 i386 s390x x86_64')

  def testcase_6(ctx):
    tp = 'base=sles(major=11,minor=sp3);arch=[i386,s390x,x86_64];addon=sdk(major=11,minor=sp3)'
    a = ctx.make_attrs(tp)
    eq_(a.product, 'sles')
    eq_(a.major, '11')
    eq_(a.minor, 'sp3')
    eq_(a.archs, ['i386', 's390x', 'x86_64'])
    eq_(a.addons, dict(sdk = dict(major = '11', minor = 'sp3')))
    eq_(a.virtual, dict(hypervisor = '', mode = ''))
    eq_(str(a), 'sles 11sp3 i386 s390x x86_64 sdk 11.sp3')

  def testcase_7(ctx):
    tp = 'base=sles(major=11,minor=sp3);arch=[x86_64,s390x];addon=sdk(major=11,minor=sp3)'
    a = ctx.make_attrs(tp)
    eq_(a.product, 'sles')
    eq_(a.major, '11')
    eq_(a.minor, 'sp3')
    eq_(a.archs, ['s390x', 'x86_64'])
    eq_(a.addons, dict(sdk = dict(major = '11', minor = 'sp3')))
    eq_(a.virtual, dict(hypervisor = '', mode = ''))
    eq_(str(a), 'sles 11sp3 s390x x86_64 sdk 11.sp3')

  def testcase_8(ctx):
    tp = 'base=sles(major=11,minor=sp3);arch=[x86_64];addon=cloud(major=3,minor=0)'
    a = ctx.make_attrs(tp)
    eq_(a.product, 'sles')
    eq_(a.major, '11')
    eq_(a.minor, 'sp3')
    eq_(a.archs, ['x86_64'])
    eq_(a.addons, dict(cloud = dict(major = '3', minor = '0')))
    eq_(a.virtual, dict(hypervisor = '', mode = ''))
    eq_(str(a), 'sles 11sp3 x86_64 cloud 3.0')

  def testcase_9(ctx):
    tp = 'base=sles(major=11,minor=sp4);arch=[x86_64,s390x];addon=sdk(major=11,minor=sp4)'
    a = ctx.make_attrs(tp)
    eq_(a.product, 'sles')
    eq_(a.major, '11')
    eq_(a.minor, 'sp4')
    eq_(a.archs, ['s390x', 'x86_64'])
    eq_(a.addons, dict(sdk = dict(major = '11', minor = 'sp4')))
    eq_(a.virtual, dict(hypervisor = '', mode = ''))
    eq_(str(a), 'sles 11sp4 s390x x86_64 sdk 11.sp4')

  def testcase_10(ctx):
    tp = 'base=sles(major=11,minor=sp4);arch=[x86_64]'
    a = ctx.make_attrs(tp)
    eq_(a.product, 'sles')
    eq_(a.major, '11')
    eq_(a.minor, 'sp4')
    eq_(a.archs, ['x86_64'])
    eq_(a.addons, dict())
    eq_(a.virtual, dict(hypervisor = '', mode = ''))
    eq_(str(a), 'sles 11sp4 x86_64')

  def testcase_11(ctx):
    tp = 'base=sles(major=12,minor=);arch=[x86_64,s390x]'
    a = ctx.make_attrs(tp)
    eq_(a.product, 'sles')
    eq_(a.major, '12')
    eq_(a.minor, '')
    eq_(a.archs, ['s390x', 'x86_64'])
    eq_(a.addons, dict())
    eq_(a.virtual, dict(hypervisor = '', mode = ''))
    eq_(str(a), 'sles 12 s390x x86_64')

  def testcase_12(ctx):
    tp = 'base=sles(major=12,minor=);arch=[x86_64,s390x];addon=bsk(major=12,minor=)'
    a = ctx.make_attrs(tp)
    eq_(a.product, 'sles')
    eq_(a.major, '12')
    eq_(a.minor, '')
    eq_(a.archs, ['s390x', 'x86_64'])
    eq_(a.addons, dict(bsk = dict(major = '12', minor = '')))
    eq_(a.virtual, dict(hypervisor = '', mode = ''))
    eq_(str(a), 'sles 12 s390x x86_64 bsk 12.')

  def testcase_13(ctx):
    tp = 'base=sles(major=12,minor=);arch=[x86_64,s390x];virtual=(mode=guest,hypervisor=kvm)'
    a = ctx.make_attrs(tp)
    eq_(a.product, 'sles')
    eq_(a.major, '12')
    eq_(a.minor, '')
    eq_(a.archs, ['s390x', 'x86_64'])
    eq_(a.addons, dict())
    eq_(a.virtual, dict(hypervisor = 'kvm', mode = 'guest'))
    eq_(str(a), 'sles 12 s390x x86_64 guest kvm')

  def testcase_14(ctx):
    tp = 'base=sles(major=12,minor=);arch=[x86_64,s390x];virtual=(mode=host,hypervisor=kvm)'
    a = ctx.make_attrs(tp)
    eq_(a.product, 'sles')
    eq_(a.major, '12')
    eq_(a.minor, '')
    eq_(a.archs, ['s390x', 'x86_64'])
    eq_(a.addons, dict())
    eq_(a.virtual, dict(hypervisor = 'kvm', mode = 'host'))
    eq_(str(a), 'sles 12 s390x x86_64 host kvm')

  def testcase_15(ctx):
    tp = 'base=sles(major=12,minor=);arch=[x86_64];addon=we(major=12,minor=)'
    a = ctx.make_attrs(tp)
    eq_(a.product, 'sles')
    eq_(a.major, '12')
    eq_(a.minor, '')
    eq_(a.archs, ['x86_64'])
    eq_(a.addons, dict(we = dict(major = '12', minor = '')))
    eq_(a.virtual, dict(hypervisor = '', mode = ''))
    eq_(str(a), 'sles 12 x86_64 we 12.')
