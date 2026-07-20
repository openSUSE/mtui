#
# spec file for package mtui
#
# Copyright (c) 2026 SUSE LLC
#
# All modifications and additions to the file contributed by third parties
# remain the property of their copyright owners, unless otherwise agreed
# upon. The license for this file, and modifications and additions to the
# file, is the same license as for the pristine package itself (unless the
# license for the pristine package is not an Open Source License, in which
# case the license is the MIT License). An "Open Source License" is a
# license that conforms to the Open Source Definition (Version 1.9)
# published by the Open Source Initiative.
#
# Please submit bugfixes or comments via https://github.com/openSUSE/mtui
#


# System Vim runtime addon dir for the vim-plugin subpackage (matches upstream mtui).
%global vimplugin_dir %{_datadir}/vim/site

Name:           mtui
# Version is filled in by the set_version source service from the git tag.
Version:        0
Release:        0
Summary:        Rust successor to the Maintenance Test Update Installer
License:        GPL-2.0-only
URL:            https://github.com/openSUSE/mtui
Source0:        %{name}-%{version}.tar.zst
Source1:        vendor.tar.zst
BuildRequires:  cargo
# >= 1.2.0 for workspace support (%%cargo_build --all, per-package %%cargo_install -p).
BuildRequires:  cargo-packaging >= 1.2.0
BuildRequires:  zstd
ExclusiveArch:  %{rust_tier1_arches}
# Optional runtime tools; mtui degrades gracefully when they are absent.
Recommends:     subversion
# Any of these terminal emulators satisfies the `terms`/`switch` launchers.
Recommends:     (gnome-terminal or konsole or sakura or rxvt-unicode or xterm or tmux or screen)

%description
An improved, idiomatic Rust successor to MTUI — the Maintenance Test Update
Installer, SUSE QE's tool for validating maintenance updates: load a request by
RRID, install and test it on reference hosts over SSH in parallel, then approve
or reject. It drives osc/svn/Gitea and openQA/QEM natively under the hood.

This package ships two static binaries: %{name}'s `mtui` interactive REPL and
the `mtui-mcp` Model Context Protocol server.

%package vim-plugin
Summary:        VIM plugin with test report syntax
Supplements:    (mtui and vim)
BuildArch:      noarch

%description vim-plugin
This plugin provides syntax highlighting and filetype detection for editing QAM
test reports (the mtui testreport/export text format).

%prep
# -a1 extracts vendor.tar.zst, placing .cargo/config + Cargo.lock + vendor/.
%autosetup -p1 -a1

%build
# `mcp` feature enables the mtui-mcp server (see docs/src/installation.md).
%{cargo_build} -p mtui-cli
%{cargo_build} -p mtui-mcp --features mcp

%install
# The shipped binaries are `mtui` / `mtui-mcp` ([[bin]] names), not the crate
# names. Install them explicitly to avoid any %%cargo_install name-keying
# surprise across cargo-packaging versions.
install -Dm755 target/release/mtui     %{buildroot}%{_bindir}/mtui
install -Dm755 target/release/mtui-mcp %{buildroot}%{_bindir}/mtui-mcp

# Shell completions (pre-generated, checked into dist/completions/).
install -Dm644 dist/completions/bash/mtui.bash     %{buildroot}%{_datadir}/bash-completion/completions/mtui
install -Dm644 dist/completions/bash/mtui-mcp.bash %{buildroot}%{_datadir}/bash-completion/completions/mtui-mcp
install -Dm644 dist/completions/zsh/_mtui          %{buildroot}%{_datadir}/zsh/site-functions/_mtui
install -Dm644 dist/completions/zsh/_mtui-mcp      %{buildroot}%{_datadir}/zsh/site-functions/_mtui-mcp
install -Dm644 dist/completions/fish/mtui.fish     %{buildroot}%{_datadir}/fish/vendor_completions.d/mtui.fish
install -Dm644 dist/completions/fish/mtui-mcp.fish %{buildroot}%{_datadir}/fish/vendor_completions.d/mtui-mcp.fish

# Man pages (pre-generated into dist/man/).
install -Dm644 dist/man/mtui.1     %{buildroot}%{_mandir}/man1/mtui.1
install -Dm644 dist/man/mtui-mcp.1 %{buildroot}%{_mandir}/man1/mtui-mcp.1

# Terminal-launcher scripts for `terms`/`switch` (shared datadir).
install -Dm755 dist/terms/*.sh -t %{buildroot}%{_datadir}/mtui/terms/

# Fully-commented example config, installed as documentation.
install -Dm644 dist/mtui.toml.example %{buildroot}%{_docdir}/%{name}/mtui.toml.example

# Vim plugin: filetype detection + testreport syntax (vim-plugin subpackage).
install -d %{buildroot}%{vimplugin_dir}/ftdetect
install -d %{buildroot}%{vimplugin_dir}/syntax
install -pm 0644 dist/vim-plugin/ftdetect/testreport.vim %{buildroot}%{vimplugin_dir}/ftdetect
install -pm 0644 dist/vim-plugin/syntax/testreport.vim   %{buildroot}%{vimplugin_dir}/syntax

%check
# The full suite needs the SSH integration fixture and network mocks; skip it in
# the offline build worker and rely on the CI gate for behavioral coverage.

%files
%license LICENSE
%doc README.md
%doc %{_docdir}/%{name}/mtui.toml.example
%{_bindir}/mtui
%{_bindir}/mtui-mcp
%dir %{_datadir}/mtui
%dir %{_datadir}/mtui/terms
%{_datadir}/mtui/terms/term.*.sh
%{_datadir}/bash-completion/completions/mtui
%{_datadir}/bash-completion/completions/mtui-mcp
%{_datadir}/zsh/site-functions/_mtui
%{_datadir}/zsh/site-functions/_mtui-mcp
%{_datadir}/fish/vendor_completions.d/mtui.fish
%{_datadir}/fish/vendor_completions.d/mtui-mcp.fish
%{_mandir}/man1/mtui.1%{?ext_man}
%{_mandir}/man1/mtui-mcp.1%{?ext_man}

%files vim-plugin
%dir %{_datadir}/vim
%dir %{vimplugin_dir}
%dir %{vimplugin_dir}/ftdetect
%dir %{vimplugin_dir}/syntax
%{vimplugin_dir}/ftdetect/testreport.vim
%{vimplugin_dir}/syntax/testreport.vim

%changelog
