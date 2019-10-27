%global _summary SuperMicro IPMI fan control daemon

Name:           ipmi-fan-control
Version:        0.2.0
Release:        1%{?dist}
Summary:        %{_summary}

# Upstream license specification: GPL-3.0-or-later
License:        GPLv3+
URL:            https://github.com/chenxiaolong/%{name}
Source:         https://github.com/chenxiaolong/%{name}/archive/v%{version}.tar.gz

ExclusiveArch:  %{rust_arches}

# We're explicitly not using the macros from here because we want to download
# dependencies from the internet
BuildRequires:  rust-packaging
BuildRequires:  systemd-rpm-macros

Requires:       ipmitool

%description
%{_summary}

%prep
%autosetup -p1

%build
export RUSTFLAGS="%{__global_rustflags}"
cargo build --release

%install
install -D -m 0755 target/release/%{name} \
    %{buildroot}%{_bindir}/%{name}

# systemd service
install -d -m 0755 %{buildroot}%{_unitdir}
cat > %{buildroot}%{_unitdir}/%{name}.service << EOF
[Unit]
Description=%{_summary}

[Service]
ExecStart=%{_bindir}/%{name} -c %{_sysconfdir}/%{name}.toml
Restart=on-failure
KillMode=process

[Install]
WantedBy=multi-user.target
EOF

%post
%systemd_post %{name}.service

%preun
%systemd_preun %{name}.service

%postun
%systemd_postun_with_restart %{name}.service

%files
%doc README.md
%doc config.sample.toml
%license LICENSE
%{_bindir}/%{name}
%{_unitdir}/%{name}.service

%changelog
* Sat Oct 26 2019 Andrew Gunnerson <andrewgunnerson@gmail.com> - 0.2.0-1
- Add support for multiple temperature/dcycle steps
- Add config file validation

* Thu Oct 24 2019 Andrew Gunnerson <andrewgunnerson@gmail.com> - 0.1.1-1
- Set KillMode=process

* Thu Oct 24 2019 Andrew Gunnerson <andrewgunnerson@gmail.com> - 0.1.0-1
- Initial package
