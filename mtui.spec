#
# spec file for package mtui
#
# Copyright (c) 2012 SUSE LINUX Products GmbH, Nuernberg, Germany.
#
# All modifications and additions to the file contributed by third parties
# remain the property of their copyright owners, unless otherwise agreed
# upon. The license for this file, and modifications and additions to the
# file, is the same license as for the pristine package itself (unless the
# license for the pristine package is not an Open Source License, in which
# case the license is the MIT License). An "Open Source License" is a
# license that conforms to the Open Source Definition (Version 1.9)
# published by the Open Source Initiative.




Name:           mtui
Version:        1.0
Release:        0.1.1
Summary:        Maintenance Test Update Installer
Group:          Productivity/Other
License:        GPL
Url:            http://www.suse.com
BuildRoot:      %{_tmppath}/%{name}-%{version}-build
BuildArch:      noarch
BuildRequires:  python-devel
Recommends:     python-notify
Requires:       python, python-paramiko, rpm-python, osc
%{!?python_sitelib: %define python_sitelib %(%{__python} -c "from distutils.sysconfig import get_python_lib; print get_python_lib()")}


%description
SUSE QA Maintenance update installer


Authors:
--------
    Christian Kornacker <ckornacker@suse.de>

%prep
cp -a $RPM_SOURCE_DIR/mtui $RPM_BUILD_DIR/
cp -a $RPM_SOURCE_DIR/helper $RPM_BUILD_DIR/
cp -a $RPM_SOURCE_DIR/scripts $RPM_BUILD_DIR/
cp -a $RPM_SOURCE_DIR/setup.py $RPM_BUILD_DIR/
cp -a $RPM_SOURCE_DIR/mtui.py $RPM_BUILD_DIR/
cp -a $RPM_SOURCE_DIR/term.gnome.sh $RPM_BUILD_DIR/
cp -a $RPM_SOURCE_DIR/term.kde.sh $RPM_BUILD_DIR/
cp -a $RPM_SOURCE_DIR/term.xterm.sh $RPM_BUILD_DIR/
cp -a $RPM_SOURCE_DIR/refhosts.xml $RPM_BUILD_DIR/
cp -a $RPM_SOURCE_DIR/README $RPM_BUILD_DIR/
cp -a $RPM_SOURCE_DIR/FAQ $RPM_BUILD_DIR/
cp -a $RPM_SOURCE_DIR/mtui.cfg.example $RPM_BUILD_DIR/
cp -a $RPM_SOURCE_DIR/prerun.example $RPM_BUILD_DIR/

%build
python setup.py build

%install
python setup.py install --prefix=%{_prefix} --root=%{buildroot}
ln -s mtui.py %{buildroot}/%{_bindir}/mtui
mkdir -p %{buildroot}%{_sysconfdir}

cat <<EOF > %{buildroot}%{_sysconfdir}/mtui.cfg
[mtui]
datadir = %{_datadir}/mtui
EOF

mkdir -p %{buildroot}%{_datadir}/mtui
cp -a scripts %{buildroot}%{_datadir}/mtui/
cp -a helper %{buildroot}%{_datadir}/mtui/
install -Dm0755 term.gnome.sh term.kde.sh term.xterm.sh %{buildroot}%{_datadir}/mtui/
install -Dm0755 refhosts.xml %{buildroot}%{_datadir}/mtui/

%clean
rm -rf %{buildroot}

%files
%defattr(-,root,root,-)
%doc README FAQ mtui.cfg.example prerun.example
%{_bindir}/mtui*
%{_datadir}/mtui/*
%dir %{_datadir}/mtui
%config %{_sysconfdir}/mtui.cfg
%{python_sitelib}/mtui*

%changelog
* Fri Sep 28 2012 ckornacker@suse.de
- version 1.0
- initial rpm version
