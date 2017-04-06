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
  eq_(a.product, {})
  eq_(a.kernel, {})
  eq_(a.ltss, {})
  eq_(a.minimal, None)

  eq_(a.arch, '')
  eq_(a.addons, [])
  eq_(a.virtual, {})

  eq_(str(a), '')

  eq_(bool(a), False)


class Test_stringification(object):
  def make_attrs(t):
        return Attributes()

  def testcase_1(ctx):
    a = ctx.make_attrs()
    a.product['name'] = 'sturd'
    a.product['version'] = {'major':42, 'minor':69}

    eq_(str(a), 'sturd 42.69')

    a.kernel = {'enabled': True}
    a.ltss = {'enabled': True}
    a.minimal = True
    eq_(str(a), 'sturd 42.69 kernel ltss minimal')

    a.virtual['mode'] = 'guest'
    eq_(str(a), 'sturd 42.69 kernel ltss minimal guest')

    a.virtual['hypervisor'] = 'rofl.pam.suici.de'
    eq_(str(a), 'sturd 42.69 kernel ltss minimal guest rofl.pam.suici.de')

    a.kernel = {'enabled': False}
    eq_(str(a), 'sturd 42.69 ltss minimal guest rofl.pam.suici.de')

    a.ltss = {'enabled': False}
    eq_(str(a), 'sturd 42.69 minimal guest rofl.pam.suici.de')

  def testcase_2(ctx):
    a = ctx.make_attrs()
    a.addons = [{'name':'foo', 'version':{'major':12, 'minor':34}},
      {'name':'qux', 'version':{'major':21}},
      {'name':'bar', 'version':{'major':45, 'minor':67}},
      {'name':'zal', 'version':{'major':43}}]

    eq_(str(a), 'bar 45.67 foo 12.34 qux 21. zal 43.')


class Test_from_testplatform(object):
  def make_attrs(ctx, tp):
    return Attributes.from_testplatform(tp, make_logger())

  def testcase_2(ctx):
    tp = 'base=sap-aio(major=12,minor=);arch=[x86_64]'
    la = ctx.make_attrs(tp)

    a = la[0]
    eq_(len(la), 1)
    eq_(a.product['name'], 'sap-aio')
    eq_(a.product['version'], {'major':12, 'minor':''})
    eq_(a.arch, 'x86_64')
    eq_(a.addons, list())
    eq_(a.virtual, {})
    eq_(str(a), 'sap-aio 12 x86_64')

  def testcase_3(ctx):
    tp = 'base=sled(major=11,minor=sp3);arch=[i386,x86_64]'
    la = ctx.make_attrs(tp)
    eq_(len(la), 2)

    a = la[0]
    eq_(a.product['name'], 'sled')
    eq_(a.product['version'], {'major':11, 'minor':'sp3'})
    eq_(a.arch, 'i386')
    eq_(a.addons, list())
    eq_(a.virtual, {})
    eq_(str(a), 'sled 11sp3 i386')

    a = la[1]
    eq_(a.product['name'], 'sled')
    eq_(a.product['version'], {'major':11, 'minor':'sp3'})
    eq_(a.arch, 'x86_64')
    eq_(a.addons, list())
    eq_(a.virtual, {})
    eq_(str(a), 'sled 11sp3 x86_64')

  def testcase_4(ctx):
    tp = 'base=sled(major=12,minor=);arch=[x86_64]'
    la = ctx.make_attrs(tp)
    a = la[0]
    eq_(len(la), 1)
    eq_(a.product['name'], 'sled')
    eq_(a.product['version'], {'major':12, 'minor':''})
    eq_(a.arch, 'x86_64')
    eq_(a.addons, list())
    eq_(a.virtual, {})
    eq_(str(a), 'sled 12 x86_64')

  def testcase_5(ctx):
    tp = 'base=sled(major=12,minor=);arch=[x86_64];virtual=(hypervisor=kvm)'
    la = ctx.make_attrs(tp)
    a = la[0]
    eq_(len(la), 1)
    eq_(a.product['name'], 'sled')
    eq_(a.product['version'], {'major':12 , 'minor':''})
    eq_(a.arch, 'x86_64')
    eq_(a.addons, list())
    eq_(a.virtual, {'hypervisor':'kvm'})
    eq_(str(a), 'sled 12 x86_64 kvm')

  def testcase_6(ctx):
    tp = 'base=sles(major=11,minor=sp3);arch=[i386,s390x,x86_64]'
    la = ctx.make_attrs(tp)
    eq_(len(la), 3)

    a = la[0]
    eq_(a.product['name'], 'sles')
    eq_(a.product['version'], {'major':11, 'minor':'sp3'})
    eq_(a.arch, 'i386')
    eq_(a.addons, list())
    eq_(a.virtual, {})
    eq_(str(a), 'sles 11sp3 i386')

    a = la[1]
    eq_(a.product['name'], 'sles')
    eq_(a.product['version'], {'major':11, 'minor':'sp3'})
    eq_(a.arch, 's390x')
    eq_(a.addons, list())
    eq_(a.virtual, {})
    eq_(str(a), 'sles 11sp3 s390x')

    a = la[2]
    eq_(a.product['name'], 'sles')
    eq_(a.product['version'], {'major':11, 'minor':'sp3'})
    eq_(a.arch, 'x86_64')
    eq_(a.addons, list())
    eq_(a.virtual, {})
    eq_(str(a), 'sles 11sp3 x86_64')


  def testcase_7(ctx):
    tp = 'base=sles(major=11,minor=sp3);arch=[i386,s390x,x86_64];addon=sdk(major=11,minor=sp3)'
    la = ctx.make_attrs(tp)
    eq_(len(la), 3)

    a = la[0]
    eq_(a.product['name'], 'sles')
    eq_(a.product['version'], {'major':11, 'minor':'sp3'})
    eq_(a.arch, 'i386')
    eq_(a.addons, [{'name': 'sdk', 'version':{'major': 11, 'minor': 'sp3'}}])
    eq_(a.virtual, {})
    eq_(str(a), 'sles 11sp3 i386 sdk 11.sp3')

    a = la[1]
    eq_(a.product['name'], 'sles')
    eq_(a.product['version'], {'major':11, 'minor':'sp3'})
    eq_(a.arch, 's390x')
    eq_(a.addons, [{'name': 'sdk', 'version':{'major': 11, 'minor': 'sp3'}}])
    eq_(a.virtual, {})
    eq_(str(a), 'sles 11sp3 s390x sdk 11.sp3')

    a = la[2]
    eq_(a.product['name'], 'sles')
    eq_(a.product['version'], {'major':11, 'minor':'sp3'})
    eq_(a.arch, 'x86_64')
    eq_(a.addons, [{'name': 'sdk', 'version':{'major': 11, 'minor': 'sp3'}}])
    eq_(a.virtual, {})
    eq_(str(a), 'sles 11sp3 x86_64 sdk 11.sp3')


  def testcase_8(ctx):
    tp = 'base=sles(major=11,minor=sp3);arch=[x86_64,s390x];addon=sdk(major=11,minor=sp3)'
    la = ctx.make_attrs(tp)
    eq_(len(la), 2)

    a = la[0]
    eq_(a.product['name'], 'sles')
    eq_(a.product['version'], {'major':11, 'minor':'sp3'})
    eq_(a.arch, 'x86_64')
    eq_(a.addons, [{'name':'sdk', 'version':{'major':11, 'minor':'sp3'}}])
    eq_(a.virtual, {})
    eq_(str(a), 'sles 11sp3 x86_64 sdk 11.sp3')

    a = la[1]
    eq_(a.product['name'], 'sles')
    eq_(a.product['version'], {'major':11, 'minor':'sp3'})
    eq_(a.arch, 's390x')
    eq_(a.addons, [{'name':'sdk', 'version':{'major':11, 'minor':'sp3'}}])
    eq_(a.virtual, {})
    eq_(str(a), 'sles 11sp3 s390x sdk 11.sp3')



  def testcase_9(ctx):
    tp = 'base=sles(major=11,minor=sp3);arch=[x86_64];addon=cloud(major=3,minor=0)'
    la = ctx.make_attrs(tp)
    a = la[0]
    eq_(len(la), 1)
    eq_(a.product['name'], 'sles')
    eq_(a.product['version'], {'major':11, 'minor':'sp3'})
    eq_(a.arch, 'x86_64')
    eq_(a.addons, [{'name': 'cloud', 'version': {'major':3, 'minor':0} }])
    eq_(a.virtual, {})
    eq_(str(a), 'sles 11sp3 x86_64 cloud 3.0')

  def testcase_10(ctx):
    tp = 'base=sles(major=11,minor=sp4);arch=[x86_64,s390x];addon=sdk(major=11,minor=sp4)'
    la = ctx.make_attrs(tp)
    eq_(len(la), 2)

    a = la[0]
    eq_(a.product['name'], 'sles')
    eq_(a.product['version'], {'major':11, 'minor':'sp4'})
    eq_(a.arch, 'x86_64')
    eq_(a.addons, [{'name':'sdk', 'version': {'major':11, 'minor': 'sp4'}}])
    eq_(a.virtual, {})
    eq_(str(a), 'sles 11sp4 x86_64 sdk 11.sp4')

    a = la[1]
    eq_(a.product['name'], 'sles')
    eq_(a.product['version'], {'major':11, 'minor':'sp4'})
    eq_(a.arch, 's390x')
    eq_(a.addons, [{'name':'sdk', 'version': {'major':11, 'minor': 'sp4'}}])
    eq_(a.virtual, {})
    eq_(str(a), 'sles 11sp4 s390x sdk 11.sp4')

  def testcase_11(ctx):
    tp = 'base=sles(major=11,minor=sp4);arch=[x86_64]'
    la = ctx.make_attrs(tp)
    a = la[0]
    eq_(len(la), 1)
    eq_(a.product['name'], 'sles')
    eq_(a.product['version'], {'major':11, 'minor':'sp4'})

    eq_(a.arch, 'x86_64')
    eq_(a.addons, list())
    eq_(a.virtual, dict())
    eq_(str(a), 'sles 11sp4 x86_64')

  def testcase_12(ctx):
    tp = 'base=sles(major=12,minor=);arch=[x86_64,s390x]'
    la = ctx.make_attrs(tp)
    eq_(len(la), 2)

    a = la[0]
    eq_(a.product['name'], 'sles')
    eq_(a.product['version'], {'major':12, 'minor':''})
    eq_(a.arch, 'x86_64')
    eq_(a.addons, list())
    eq_(a.virtual, dict())
    eq_(str(a), 'sles 12 x86_64')

    a = la[1]
    eq_(a.product['name'], 'sles')
    eq_(a.product['version'], {'major':12, 'minor':''})
    eq_(a.arch, 's390x')
    eq_(a.addons, list())
    eq_(a.virtual, dict())
    eq_(str(a), 'sles 12 s390x')

  def testcase_13(ctx):
    tp = 'base=sles(major=12,minor=);arch=[x86_64,s390x];addon=bsk(major=12,minor=)'
    la = ctx.make_attrs(tp)
    eq_(len(la), 2)

    a = la[0]
    eq_(a.product['name'], 'sles')
    eq_(a.product['version'], {'major':12, 'minor':''})
    eq_(a.arch, 'x86_64')
    eq_(a.addons, [{'name':'bsk', 'version':{'major':12, 'minor':''}}])
    eq_(a.virtual, dict())
    eq_(str(a), 'sles 12 x86_64 bsk 12.')

    a = la[1]
    eq_(a.product['name'], 'sles')
    eq_(a.product['version'], {'major':12, 'minor':''})
    eq_(a.arch, 's390x')
    eq_(a.addons, [{'name':'bsk', 'version':{'major':12, 'minor':''}}])
    eq_(a.virtual, dict())
    eq_(str(a), 'sles 12 s390x bsk 12.')

  def testcase_14(ctx):
    tp = 'base=sles(major=12,minor=);arch=[x86_64,s390x];virtual=(mode=guest,hypervisor=kvm)'
    la = ctx.make_attrs(tp)
    eq_(len(la), 2)

    a = la[0]
    eq_(a.product['name'], 'sles')
    eq_(a.product['version'], {'major':12, 'minor':''})
    eq_(a.arch, 'x86_64')
    eq_(a.addons, list())
    eq_(a.virtual, {'hypervisor': 'kvm', 'mode': 'guest'})
    eq_(str(a), 'sles 12 x86_64 guest kvm')

    a = la[1]
    eq_(a.product['name'], 'sles')
    eq_(a.product['version'], {'major':12, 'minor':''})
    eq_(a.arch, 's390x')
    eq_(a.addons, list())
    eq_(a.virtual, {'hypervisor': 'kvm', 'mode': 'guest'})
    eq_(str(a), 'sles 12 s390x guest kvm')

  def testcase_15(ctx):
    tp = 'base=sles(major=12,minor=);arch=[x86_64,s390x];virtual=(mode=host,hypervisor=kvm)'
    la = ctx.make_attrs(tp)
    eq_(len(la), 2)

    a = la[0]
    eq_(a.product['name'], 'sles')
    eq_(a.product['version'], {'major':12, 'minor':''})
    eq_(a.arch, 'x86_64')
    eq_(a.addons, list())
    eq_(a.virtual, {'hypervisor': 'kvm', 'mode': 'host'})
    eq_(str(a), 'sles 12 x86_64 host kvm')

    a = la[1]
    eq_(a.product['name'], 'sles')
    eq_(a.product['version'], {'major':12, 'minor':''})
    eq_(a.arch, 's390x')
    eq_(a.addons, list())
    eq_(a.virtual, {'hypervisor': 'kvm', 'mode': 'host'})
    eq_(str(a), 'sles 12 s390x host kvm')

  def testcase_16(ctx):
    tp = 'base=sles(major=12,minor=);arch=[x86_64];addon=we(major=12,minor=)'
    la = ctx.make_attrs(tp)
    a = la[0]
    eq_(len(la), 1)
    eq_(a.product['name'], 'sles')
    eq_(a.product['version'], {'major':12, 'minor':''})
    eq_(a.arch, 'x86_64')
    eq_(a.addons, [{'name': 'we', 'version':{'major':12, 'minor':''}}])
    eq_(a.virtual, {})
    eq_(str(a), 'sles 12 x86_64 we 12.')

  def testcase_17(ctx):
    tp = 'base=sles(major=12,minor=);arch=[s390x,x86_64]'
    la = ctx.make_attrs(tp)
    eq_(len(la), 2)

    a = la[0]
    eq_(a.product['name'], 'sles')
    eq_(a.product['version'], {'major':12, 'minor':''})
    eq_(a.arch, 's390x')
    eq_(a.addons, list())

    a = la[1]
    eq_(a.product['name'], 'sles')
    eq_(a.product['version'], {'major':12, 'minor':''})
    eq_(a.arch, 'x86_64')
    eq_(a.addons, list())

  def testcase_18(ctx):
    tp = 'base=sles(major=12);arch=[s390x,x86_64]'
    la = ctx.make_attrs(tp)
    eq_(len(la), 2)

    a = la[0]
    eq_(a.product['name'], 'sles')
    eq_(a.product['version'], {'major':12})
    eq_(a.arch, 's390x')
    eq_(a.addons, list())
    assert('minor' not in a.product['version'])

    a = la[1]
    eq_(a.product['name'], 'sles')
    eq_(a.product['version'], {'major':12})
    eq_(a.arch, 'x86_64')
    eq_(a.addons, list())
    assert('minor' not in a.product['version'])
