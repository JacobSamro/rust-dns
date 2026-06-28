#!/bin/sh
# rust-dns installer for Linux — fully managed.
#
# Downloads the latest release, verifies its checksum, installs into
# /opt/rust-dns, frees port 53 from systemd-resolved if needed, then starts and
# enables the service and prints your admin token. Re-running it upgrades in
# place without touching your config or blocklist.
#
#   curl -fsSL https://raw.githubusercontent.com/JacobSamro/rust-dns/master/install.sh | sh
#
# Env overrides: PREFIX=/opt/rust-dns  REPO=JacobSamro/rust-dns  NO_START=1
set -eu

REPO="${REPO:-JacobSamro/rust-dns}"
PREFIX="${PREFIX:-/opt/rust-dns}"

say() { printf '\033[1;32m==>\033[0m %s\n' "$1"; }
warn() { printf '\033[1;33mwarn:\033[0m %s\n' "$1" >&2; }
err() {
	printf '\033[1;31merror:\033[0m %s\n' "$1" >&2
	exit 1
}

[ "$(uname -s)" = "Linux" ] || err "this installer is Linux-only. On other systems build from source: https://github.com/$REPO#contributing"

case "$(uname -m)" in
x86_64 | amd64) target="x86_64-unknown-linux-gnu" ;;
*) err "no prebuilt binary for $(uname -m) yet. Build from source: https://github.com/$REPO#contributing" ;;
esac

command -v systemctl >/dev/null 2>&1 || err "this installer needs systemd"

# Need a downloader.
if command -v curl >/dev/null 2>&1; then
	dl() { curl -fsSL "$1"; }
	dlo() { curl -fsSL -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
	dl() { wget -qO- "$1"; }
	dlo() { wget -qO "$2" "$1"; }
else
	err "need curl or wget"
fi

# Run privileged steps with sudo unless we are already root.
if [ "$(id -u)" -eq 0 ]; then
	SUDO=""
else
	command -v sudo >/dev/null 2>&1 || err "need root, or install sudo"
	SUDO="sudo"
fi

# ---- download ----

say "Finding the latest release of $REPO ..."
tag=$(dl "https://api.github.com/repos/$REPO/releases/latest" |
	grep -o '"tag_name"[ ]*:[ ]*"[^"]*"' | head -n1 | cut -d'"' -f4)
[ -n "$tag" ] || err "could not determine the latest release tag"
say "Latest release is $tag"

name="rust-dns-$tag-$target"
base="https://github.com/$REPO/releases/download/$tag"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

say "Downloading $name.tar.gz ..."
dlo "$base/$name.tar.gz" "$tmp/$name.tar.gz"
dlo "$base/$name.tar.gz.sha256" "$tmp/$name.tar.gz.sha256" || true

if [ -s "$tmp/$name.tar.gz.sha256" ] && command -v sha256sum >/dev/null 2>&1; then
	say "Verifying checksum ..."
	(cd "$tmp" && sha256sum -c "$name.tar.gz.sha256" >/dev/null) || err "checksum mismatch — refusing to install"
else
	warn "skipping checksum (no sha256sum or checksum file)"
fi

tar xzf "$tmp/$name.tar.gz" -C "$tmp"
src="$tmp/$name"

# ---- install files (never clobber config/blocklist) ----

say "Installing to $PREFIX ..."
$SUDO mkdir -p "$PREFIX"
$SUDO cp "$src/rust-dns" "$PREFIX/rust-dns.new"
$SUDO chmod 0755 "$PREFIX/rust-dns.new"
$SUDO mv "$PREFIX/rust-dns.new" "$PREFIX/rust-dns" # atomic swap, safe while running

if [ ! -f "$PREFIX/config.toml" ]; then
	$SUDO cp "$src/config.example.toml" "$PREFIX/config.toml"
	say "Wrote starter $PREFIX/config.toml"
else
	say "Kept existing $PREFIX/config.toml"
fi
if [ ! -f "$PREFIX/blocklist.txt" ]; then
	printf 'facebook.com\n' | $SUDO tee "$PREFIX/blocklist.txt" >/dev/null
	say "Wrote starter $PREFIX/blocklist.txt"
fi

$SUDO cp "$src/rust-dns.service" /etc/systemd/system/rust-dns.service

if [ "${NO_START:-0}" = "1" ]; then
	say "NO_START set — installed but not started."
	exit 0
fi

# ---- free port 53 from systemd-resolved if it's squatting on it ----

if systemctl is-active --quiet systemd-resolved; then
	say "systemd-resolved is active; disabling its port-53 stub listener ..."
	conf=/etc/systemd/resolved.conf
	if [ -f "$conf" ] && [ ! -f "$conf.rust-dns.bak" ]; then
		$SUDO cp "$conf" "$conf.rust-dns.bak"
		say "Backed up $conf -> $conf.rust-dns.bak"
	fi
	if grep -q '^[#[:space:]]*DNSStubListener=' "$conf" 2>/dev/null; then
		$SUDO sed -i 's/^[#[:space:]]*DNSStubListener=.*/DNSStubListener=no/' "$conf"
	else
		printf 'DNSStubListener=no\n' | $SUDO tee -a "$conf" >/dev/null
	fi
	$SUDO systemctl restart systemd-resolved
	# nss-resolve keeps host name resolution working without the stub.
fi

# ---- start the service ----

say "Starting rust-dns ..."
$SUDO systemctl daemon-reload
$SUDO systemctl enable --now rust-dns

# Give it a moment to bind and (on first run) generate the admin token.
i=0
while [ "$i" -lt 25 ]; do
	systemctl is-active --quiet rust-dns || break
	tok=$($SUDO grep -E '^[[:space:]]*admin_token[[:space:]]*=' "$PREFIX/config.toml" 2>/dev/null |
		head -n1 | sed 's/.*=[[:space:]]*"\(.*\)".*/\1/')
	[ -n "${tok:-}" ] && break
	i=$((i + 1))
	sleep 0.2
done

if ! systemctl is-active --quiet rust-dns; then
	warn "service did not stay active. Recent logs:"
	$SUDO journalctl -u rust-dns -n 20 --no-pager || true
	err "rust-dns failed to start (port 53 may be held by another resolver — check 'sudo ss -lunp sport = :53')"
fi

say "rust-dns $tag is running."
printf '\n'
printf '  Admin token : %s\n' "${tok:-<see: sudo grep admin_token $PREFIX/config.toml>}"
printf '  Portal      : http://127.0.0.1:8080\n'
printf '  Blocklist   : %s\n' "$PREFIX/blocklist.txt"
printf '  Logs        : sudo journalctl -u rust-dns -f\n'
printf '\nTest it:\n'
printf '  dig @127.0.0.1 facebook.com   # -> 0.0.0.0 (blocked)\n'
printf '\nPoint your router DHCP (or this host) at this machine to use it.\n'
