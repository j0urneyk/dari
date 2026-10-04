#!/usr/bin/env bash
# Lets crosscheck.sh drive dari-check on this Mac over SSH: the macOS counterpart of
# windows/prepare-peer.ps1. Turns on Remote Login with key-only login, authorizes PUBLIC_KEY for
# this user, and creates ~/dari-check. Asks for sudo.
#
# SSH alone can't capture the screen or inject input; also run `interactive.sh serve` in this
# Mac's signed-in session (see that script) for as long as the checks run.
#
#   scripts/crosscheck/macos/prepare-peer.sh 'ssh-ed25519 AAAA... me@driver'
set -euo pipefail

key=${1:-}
[[ $key =~ ^(ssh-ed25519|ecdsa-sha2-nistp256|ssh-rsa)\ [A-Za-z0-9+/=]+(\ [^[:cntrl:]]*)?$ ]] || {
  echo "usage: $0 'ssh-ed25519 AAAA... comment'" >&2
  exit 2
}

# StrictModes ignores authorized_keys under a group- or world-writable ~/.ssh, as GitHub's macOS
# image leaves it.
mkdir -p ~/.ssh
chmod 700 ~/.ssh
touch ~/.ssh/authorized_keys
chmod 600 ~/.ssh/authorized_keys
grep -qxF "$key" ~/.ssh/authorized_keys || printf '%s\n' "$key" >>~/.ssh/authorized_keys

printf 'PasswordAuthentication no\nKbdInteractiveAuthentication no\n' |
  sudo tee /etc/ssh/sshd_config.d/050-dari-check.conf >/dev/null
# Remote Login, without `systemsetup`, which needs Full Disk Access on recent macOS. sshd starts per
# connection, so the configuration above applies from the next login.
sudo launchctl enable system/com.openssh.sshd
sudo launchctl bootstrap system /System/Library/LaunchDaemons/ssh.plist 2>/dev/null || true
# With Remote Login limited to some users, this group lists them.
if dscl . -read /Groups/com.apple.access_ssh >/dev/null 2>&1; then
  sudo dseditgroup -o edit -a "$USER" -t user com.apple.access_ssh
fi
for _ in $(seq 1 20); do nc -z 127.0.0.1 22 2>/dev/null && break; sleep 0.5; done
nc -z 127.0.0.1 22 || { echo "Remote Login did not start" >&2; exit 1; }

mkdir -p ~/dari-check/logs
echo "Ready: ssh $USER@<this Mac> can drive dari-check while interactive.sh serve runs here"
