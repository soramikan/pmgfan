# cargo release ビルドには DWARF debuginfo がないため
# debuginfo/debugsource サブパッケージを作らない
%global debug_package %{nil}

Name:           pmgfan
Version:        0.1.0
Release:        1%{?dist}
Summary:        Safe fan control daemon for Fujitsu PRIMERGY TX1320 M4 (iRMC S5)

License:        Apache-2.0
URL:            https://github.com/soramikan/pmgfan
Source0:        %{name}-%{version}.tar.gz

BuildRequires:  systemd-rpm-macros
# ランタイム依存。pmgfand は ipmitool 経由で iRMC と通信する
Requires:       ipmitool
Requires(pre):  shadow-utils
%{?systemd_requires}

# cargo はディストリビューション外の toolchain（rustup 等）でも
# 使えるよう BuildRequires にはせず %build で検査する

%description
pmgfan controls the system fans of a Fujitsu PRIMERGY TX1320 M4
through its iRMC S5 BMC over local IPMI. It consists of:

* pmgfand   - privileged daemon that owns fan control and falls
              back to iRMC automatic mode on any failure
* pmgfanctl - unprivileged CLI/TUI client that talks to pmgfand
              over a Unix socket (/run/pmgfand/control.sock,
              group 'pmgfan')

%prep
%autosetup -n %{name}-%{version}

%build
if ! command -v cargo >/dev/null 2>&1; then
    for d in "$HOME/.cargo/bin" /usr/local/cargo/bin; do
        [ -x "$d/cargo" ] && PATH="$d:$PATH"
    done
fi
cargo build --release --locked

%install
install -D -m0755 target/release/pmgfand   %{buildroot}%{_sbindir}/pmgfand
install -D -m0755 target/release/pmgfanctl %{buildroot}%{_bindir}/pmgfanctl
install -D -m0644 systemd/pmgfand.service  %{buildroot}%{_unitdir}/pmgfand.service
install -d -m0750 %{buildroot}%{_sysconfdir}/pmgfand
install -D -m0640 config/pmgfand.toml \
    %{buildroot}%{_sysconfdir}/pmgfand/config.toml
install -D -m0644 LICENSE %{buildroot}%{_licensedir}/pmgfan/LICENSE

%pre
# pmgfand.socket は root:pmgfan 0660。グループが無いと
# unit の Group=pmgfan で起動自体が失敗する
getent group pmgfan >/dev/null || groupadd -r pmgfan

%post
%systemd_post %{name}.service
# 既存の手動導入（/run/pmgfand 以下）は systemd の
# RuntimeDirectory= と同じ場所を使うので追加作業は不要

%preun
%systemd_preun %{name}.service

%postun
%systemd_postun_with_restart %{name}.service

%files
%license %{_licensedir}/pmgfan/LICENSE
%{_sbindir}/pmgfand
%{_bindir}/pmgfanctl
%{_unitdir}/pmgfand.service
%dir %attr(0750,root,pmgfan) %{_sysconfdir}/pmgfand
%config(noreplace) %attr(0640,root,pmgfan) %{_sysconfdir}/pmgfand/config.toml

%changelog
* Sun Sep 28 2025 sora <soramikan> - 0.1.0-1
- Initial RPM package
