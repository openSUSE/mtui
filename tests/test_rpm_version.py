from mtui.rpmver import RPMVersion


def check_less_than(v1, v2):
    assert v1 < v2

def test_version():
    
    version_comparison = [
        ('2014.104.0.0.2svn15878-21.19', '2015.104.0.0.2svn15878-21.12'),
        ('1.2.0-7.20', '1.2.0-7.30'),
        ('0.9~20170329.eb3dfbb', '0.9~20170329.798fdeb') # This doesn't make sense. It's RPM non-sense
        ]

    for versions in version_comparison:
        smaller = versions[0]
        bigger = versions[1]
        yield check_less_than, RPMVersion(smaller), RPMVersion(bigger)
