#!/bin/sh
# Consented, fixed app asset. Executed as root with no project-provided arguments.
set -eu
[ "$(id -u)" -eq 0 ] || { echo 'Docker installation requires root in the explicitly selected distro.' >&2; exit 1; }
[ -r /etc/os-release ] || { echo 'Missing /etc/os-release; install Docker Engine manually.' >&2; exit 1; }
. /etc/os-release
case "${ID:-}" in
  ubuntu|debian) ;;
  *) echo "Automatic installation supports Ubuntu/Debian only. Install this distro's native Docker Engine (Linux daemon, docker CLI, /var/run/docker.sock), start its docker service, then Recheck. Do not enable Docker Desktop integration for this option." >&2; exit 2 ;;
esac
[ -n "${VERSION_CODENAME:-}" ] || { echo 'This distro has no VERSION_CODENAME; configure the official Docker apt repository manually.' >&2; exit 2; }
# Never remove conflicting packages or replace a Desktop integration shim silently.
for package in docker.io docker-compose docker-compose-v2 docker-doc podman-docker containerd runc; do
  if dpkg-query -W -f='${Status}' "$package" 2>/dev/null | /bin/grep -q '^install ok installed$'; then
    echo "Conflicting package $package is installed. Review Docker's official Ubuntu/Debian uninstall-conflicts instructions manually; OMP Manager will not remove it." >&2
    exit 3
  fi
done
if command -v docker >/dev/null 2>&1; then
  case "$(readlink -f "$(command -v docker)")" in
    /mnt/wsl/docker-desktop/*|/mnt/c/*|*.exe) echo "Choose Docker Desktop for this distro's Desktop socket." >&2; exit 3 ;;
  esac
fi
export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y ca-certificates curl
install -m 0755 -d /etc/apt/keyrings
key_tmp=$(mktemp)
trap 'rm -f "$key_tmp"' EXIT HUP INT TERM
curl --fail --show-error --silent --location --proto '=https' --tlsv1.2 "https://download.docker.com/linux/$ID/gpg" -o "$key_tmp"
install -m 0644 "$key_tmp" /etc/apt/keyrings/docker.asc
arch=$(dpkg --print-architecture)
printf 'Types: deb\nURIs: https://download.docker.com/linux/%s\nSuites: %s\nComponents: stable\nArchitectures: %s\nSigned-By: /etc/apt/keyrings/docker.asc\n' "$ID" "$VERSION_CODENAME" "$arch" > /etc/apt/sources.list.d/docker.sources
apt-get update
apt-get install -y docker-ce docker-ce-cli containerd.io
if command -v systemctl >/dev/null 2>&1 && [ -d /run/systemd/system ]; then
  systemctl start docker
else
  service docker start
fi
docker --host unix:///var/run/docker.sock info >/dev/null
printf 'Docker Engine installation finished in the selected distro.\n'
