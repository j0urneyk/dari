#!/usr/bin/env bash
# Creates a Windows 11 on Arm VM in UTM for the cross-device checks and installs it unattended:
# Windows itself, the UTM guest tools, then (bootstrap.ps1, at first logon) SSH, Rust, and the
# Visual Studio Build Tools. The whole install takes about an hour; wait-vm.sh reports when the
# VM is ready for crosscheck.sh.
#
# State lives in ~/.dari-check-vm: the SSH key crosscheck.sh logs in with, the VM user's
# password, the guest tools ISO, and the generated setup ISO.
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: scripts/crosscheck/vm/create-vm.sh --iso WINDOWS_ARM64.iso [options]

  --iso PATH       Windows 11 on Arm ISO from microsoft.com/software-download/windows11arm64
  --name NAME      UTM virtual machine name (default: dari-win11)
  --memory MIB     RAM (default: 8192)
  --cpus N         CPU cores (default: 4)
  --disk-gib N     System disk size (default: 96)
USAGE
}

iso='' name=dari-win11 memory=8192 cpus=4 disk_gib=96
while [[ $# -gt 0 ]]; do
  case "$1" in
    --iso) iso=$2; shift 2 ;;
    --name) name=$2; shift 2 ;;
    --memory) memory=$2; shift 2 ;;
    --cpus) cpus=$2; shift 2 ;;
    --disk-gib) disk_gib=$2; shift 2 ;;
    -h | --help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done
[[ -f $iso ]] || { usage >&2; exit 2; }
iso=$(cd "$(dirname "$iso")" && pwd)/$(basename "$iso")

here=$(cd "$(dirname "$0")" && pwd)
state=$HOME/.dari-check-vm
mkdir -p "$state"
chmod 700 "$state"

user=dari
[[ -f $state/id_ed25519 ]] || ssh-keygen -q -t ed25519 -N '' -C dari-check-vm -f "$state/id_ed25519"
if [[ ! -f $state/password ]]; then
  # Alphanumeric, so it fits the answer file as is. Not `tr </dev/urandom | head`: tr dies of
  # SIGPIPE, which pipefail turns into an exit.
  (umask 077 && openssl rand -base64 24 | tr -dc 'A-Za-z0-9' >"$state/password")
fi
password=$(cat "$state/password")
[[ -f $state/utm-guest-tools.iso ]] ||
  curl -fsSL -o "$state/utm-guest-tools.iso" https://getutm.app/downloads/utm-guest-tools-latest.iso

# The setup ISO: the guest tools (drivers for setup, the installer for first logon), the answer
# file, and the scripts bootstrap.ps1 runs.
stage=$(mktemp -d)
mount=$(mktemp -d)
trap 'hdiutil detach -quiet "$mount" 2>/dev/null || true; rm -rf "$stage" "$mount"' EXIT
hdiutil attach -quiet -nobrowse -readonly -mountpoint "$mount" "$state/utm-guest-tools.iso"
cp -R "$mount/Drivers" "$mount"/utm-guest-tools-*.exe "$stage/"
hdiutil detach -quiet "$mount"
sed -e "s/@USER@/$user/g" -e "s/@PASSWORD@/$password/g" "$here/autounattend.xml" >"$stage/autounattend.xml"
cp "$here/bootstrap.ps1" "$here"/../windows/*.ps1 "$state/id_ed25519.pub" "$stage/"
rm -f "$state/setup.iso"
hdiutil makehybrid -quiet -iso -joliet -default-volume-name DARI_SETUP -o "$state/setup.iso" "$stage"

if osascript -e "tell application \"UTM\" to get name of virtual machine \"$name\"" >/dev/null 2>&1; then
  echo "UTM already has a virtual machine named $name; delete it or pass --name" >&2
  exit 1
fi
echo "Creating $name in UTM…"
# Windows is the first CD (D:) and the setup ISO the second (E:), which the answer file's
# driver paths expect. The system disk is NVMe, which Windows setup supports without drivers.
osascript <<OSA
tell application "UTM"
  set windowsIso to POSIX file "$iso"
  set setupIso to POSIX file "$state/setup.iso"
  make new virtual machine with properties {backend:qemu, configuration:{name:"$name", architecture:"aarch64", memory:$memory, cpu cores:$cpus, hypervisor:true, uefi:true, drives:{{removable:true, source:windowsIso}, {removable:true, source:setupIso}, {interface:NVMe, guest size:$((disk_gib * 1024))}}, network interfaces:{{mode:shared}}, displays:{{hardware:"virtio-ramfb"}}}}
end tell
OSA

echo "Starting $name; pressing a key at the boot-from-CD prompt…"
osascript -e "tell application \"UTM\" to start virtual machine \"$name\""
for _ in $(seq 1 20); do
  osascript -e "tell application \"UTM\" to input keystroke virtual machine \"$name\" text \" \"" >/dev/null 2>&1 || true
  sleep 1
done
echo "Windows setup is running unattended. Run scripts/crosscheck/vm/wait-vm.sh --name $name to follow it."
