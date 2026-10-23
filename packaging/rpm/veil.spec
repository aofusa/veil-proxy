Name:           veil
Version:        %{veil_version}
Release:        1%{?dist}
Summary:        High-performance reverse proxy and web server
License:        Apache-2.0 OR MIT
URL:            https://github.com/aofusa/veil-proxy
# アーキテクチャは rpmbuild --target で指定する（build.sh が RPM_ARCH を渡す）。
# BuildArch を併記すると x86_64 ホストでの aarch64 ビルドが
# "No compatible architectures found for build" で失敗する。
Requires:       systemd
Requires(pre):  shadow-utils
AutoReqProv:    no

%description
Veil is a reverse proxy and web server for HTTP/1.1, HTTP/2 and HTTP/3. The
package installs a default configuration in /etc/veil that serves a page on
http://127.0.0.1:8080/ once the veil service is enabled and started.

%prep
%build

%install
rm -rf %{buildroot}
mkdir -p %{buildroot}
cp -a %{_sourcedir}/rootfs/. %{buildroot}/

%pre
getent group veil >/dev/null 2>&1 || groupadd --system veil
getent passwd veil >/dev/null 2>&1 || useradd --system --gid veil --home-dir /nonexistent --shell /sbin/nologin veil
exit 0

%post
VEIL_PKG_KIND=rpm /usr/share/veil/scripts/postinstall.sh "$1"

%preun
/usr/share/veil/scripts/preuninstall.sh "$1"

%postun
if [ "$1" -eq 0 ]; then
    systemctl daemon-reload >/dev/null 2>&1 || :
fi

%files
%defattr(-,root,root,-)
/usr/bin/veil
/usr/share/veil
%dir /etc/veil
%config(noreplace) %attr(0640,root,veil) /etc/veil/config.toml
/usr/lib/systemd/system/veil.service
%doc /usr/share/doc/veil/copyright

%changelog
