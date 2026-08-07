Name:           veil
Version:        %{veil_version}
Release:        1%{?dist}
Summary:        High-performance reverse proxy server using io_uring and rustls
License:        Apache-2.0 OR MIT
URL:            https://github.com/aofusa/veil
# アーキテクチャは rpmbuild --target で指定する（build.sh が RPM_ARCH を渡す）。
# BuildArch を併記すると x86_64 ホストでの aarch64 ビルドが
# "No compatible architectures found for build" で失敗する。
Requires:       openssl
Requires:       systemd
AutoReqProv:    no

%description
Veil is a Linux-native high-performance reverse proxy supporting
HTTP/1.1, HTTP/2, HTTP/3, kTLS, Proxy-Wasm, and advanced security
features (seccomp, Landlock).

%prep
%build

%install
rm -rf %{buildroot}
mkdir -p %{buildroot}
cp -a %{_sourcedir}/rootfs/. %{buildroot}/

%post
/usr/share/veil/scripts/postinstall.sh "$1"

%preun
if [ "$1" -eq 0 ]; then
    /usr/share/veil/scripts/preuninstall.sh 0
fi

%files
%defattr(-,root,root,-)
/usr/bin/veil
/usr/share/veil
/lib/systemd/system/veil.service

%changelog