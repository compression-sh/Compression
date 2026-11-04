#!/bin/sh
# Install the .deb and the .rpm from $1 (default: dist) into fresh containers that already have Nautilus, and
# start the installed binary. Nautilus ships a FileManager1 D-Bus service file of its own, so this catches
# two packages claiming the same file, and a package whose dependencies cannot be met.
# Only for CI: the containers see nothing but the packages (read-only) and the package mirrors.
set -eu
dist=$(cd "${1:-dist}" && pwd)
version=$(cargo metadata --no-deps --format-version 1 | jq -r '.packages[0].version')

# Run inside the container: the installed program must answer, and must have its own service file
check='
compression --version | grep -Fx "compression '"$version"'"
grep -q "Exec=/usr/bin/compression --portal" /usr/share/dbus-1/services/org.compression.Compression.FileManager1.service
test -f /usr/share/applications/compression.desktop
'

echo "== .deb on Ubuntu 22.04 with Nautilus"
docker run --rm -e DEBIAN_FRONTEND=noninteractive -v "$dist:/dist:ro" ubuntu:22.04 sh -ec "
  apt-get update -qq
  apt-get install -y -qq nautilus
  apt-get install -y -qq /dist/compression_${version}_amd64.deb
  $check
"

echo "== .rpm on Fedora 43 with Nautilus"
docker run --rm -v "$dist:/dist:ro" fedora:43 sh -ec "
  dnf install -y -q nautilus
  dnf install -y -q /dist/compression-${version}-1.x86_64.rpm
  $check
"
