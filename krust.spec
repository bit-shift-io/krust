%bcond_without check

Name:           krust
Version:        0.1.0
Release:        1%{?dist}
Summary:        Krust application binary

License:        MIT OR Apache-2.0
URL:            https://github.com/bit-shift-io/krust
Source0:        %{url}/archive/refs/tags/v%{version}.tar.gz

BuildRequires:  cargo-rpm-macros >= 24
BuildRequires:  gcc

%description
Krust repository build managed by bit-shift-io.

%prep
%autosetup -n %{name}-%{version}
%cargo_prep

%generate_buildrequires
%cargo_generate_buildrequires

%build
%cargo_build

%install
%cargo_install

%if %{with check}
%check
%cargo_test
%endif

%files
%license LICENSE*
%doc README*
%{_bindir}/krust

%changelog
* Thu Aug 20 2026 Bronson Mathews <bronson@localhost> - 0.1.0-1
- Initial RPM package build for krust