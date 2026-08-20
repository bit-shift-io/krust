Name:           krust
Version:        0.1.0
Release:        1%{?dist}
Summary:        Fast, single-binary web terminal emulator

License:        MIT
URL:            https://github.com/bit-shift-io/krust
# GitHub release archive for the v%{version} tag
Source0:        %{url}/archive/refs/tags/v%{version}.tar.gz

BuildRequires:  cargo
BuildRequires:  rust-src
BuildRequires:  gcc

%description
A fast, single-binary web terminal emulator built in Rust using Axum and
xterm.js. Provides a browser-based terminal with WebGL acceleration and
truecolor support.

%prep
%autosetup -p1
%cargo_prep

%build
%cargo_build

%install
%cargo_install

%files
%{_bindir}/%{name}
%license LICENSE
%doc README.md

%changelog
* Thu Aug 20 2026 Bronson Mathews <bronson@localhost> - 0.1.0-1
- Initial RPM package build for krust
