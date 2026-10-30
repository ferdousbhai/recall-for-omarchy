#!/usr/bin/env bash
# Run install.sh for real, with pacman, pacman-key, curl, gpg, runuser and
# systemd-run stubbed, inside a user and mount namespace where
# /etc/pacman.conf, /etc/pacman.d, /etc/passwd, /etc/group, /root, /run/user
# and /sys/firmware are scratch copies: nothing on this machine changes.
# Covers both architectures (vulkan-asahi only on Apple hardware), plain
# Arch, re-running, options, and the per-user `recall setup` step: run as the
# desktop user in their session, skipped without one, a failure reported, and
# a tarball install from earlier installers left for setup to take over
# (tests/desktop.rs runs that setup). First, outside the namespace, checks the
# add_signed_repo hash; only the installer runs are skipped where
# unprivileged namespaces are off.
set -euo pipefail
cd "$(dirname "$0")/.."
root="$PWD"

work="$(mktemp -d)"
trap 'rm -rf "${work:?}"' EXIT
fail=0
check() { # check <description> <command...>
  local what=$1
  shift
  if "$@"; then echo "ok $what"; else echo "FAIL $what"; fail=1; fi
}
not() { ! "$@"; }

if [[ ${RECALL_INSTALL_TEST_INNER:-} != 1 ]]; then
  hash_of() { sed -n '/^# --- add_signed_repo (shared) ---$/,/^# --- end add_signed_repo ---$/p' "$1" | sed '1d;$d' | sha256sum | cut -d' ' -f1; }
  check "add_signed_repo hash in install.sh" [ "$(hash_of install.sh)" = "$(cat tests/add_signed_repo.sha256)" ]
  if ((fail)); then
    echo "add_signed_repo drifted from its pinned hash: review the change and update tests/add_signed_repo.sha256" >&2
    exit 1
  fi
  if ! unshare -rm true 2>/dev/null; then
    echo "skip install_test: unprivileged user namespaces are unavailable" >&2
    exit 0
  fi
  rm -rf "${work:?}"
  exec env RECALL_INSTALL_TEST_INNER=1 unshare -rm "$root/tests/install_test.sh"
fi

# Ubuntu CI has no pacman mount targets. Prepare a scratch /etc inside the
# private mount namespace while preserving account lookups used by hooks.
if [[ ! -f /etc/pacman.conf || ! -d /etc/pacman.d ]]; then
  mkdir -p "$work/etc/pacman.d"
  for name in passwd group nsswitch.conf; do
    [[ ! -f /etc/$name ]] || cp "/etc/$name" "$work/etc/$name"
  done
  touch "$work/etc/pacman.conf"
  mount --bind "$work/etc" /etc
fi
# A desktop user, alice (uid 4242), whose home is scratch too.
alice_home="$work/home/alice"
cp /etc/passwd "$work/passwd"
cp /etc/group "$work/group"
echo "alice:x:4242:4242::$alice_home:/bin/bash" >>"$work/passwd"
echo "alice:x:4242:" >>"$work/group"
mount --bind "$work/passwd" /etc/passwd
mount --bind "$work/group" /etc/group
mkdir -p "$work/run-user"
mount --bind "$work/run-user" /run/user

# Stubs: pacman and the per-user commands log their arguments, curl
# "downloads" an empty key, gpg reports the pinned fingerprint for it.
mkdir -p "$work/bin"
cat >"$work/bin/pacman" <<'STUB'
#!/bin/sh
echo "pacman $*" >>"$PACMAN_LOG"
STUB
cat >"$work/bin/omarchy-pkg-add" <<'STUB'
#!/bin/sh
echo "omarchy-pkg-add $*" >>"$PACMAN_LOG"
STUB
cat >"$work/bin/pacman-key" <<'STUB'
#!/bin/sh
exit 0
STUB
cat >"$work/bin/curl" <<'STUB'
#!/bin/sh
while [ $# -gt 0 ]; do [ "$1" = -o ] && : >"$2"; shift; done
STUB
cat >"$work/bin/uname" <<'STUB'
#!/bin/sh
echo "${STUB_ARCH:-x86_64}"
STUB
cat >"$work/bin/runuser" <<'STUB'
#!/bin/sh
echo "runuser $*" >>"$SETUP_LOG"
while [ "$1" != -- ]; do shift; done
shift
exec "$@"
STUB
cat >"$work/bin/systemd-run" <<'STUB'
#!/bin/sh
echo "systemd-run XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR DBUS_SESSION_BUS_ADDRESS=$DBUS_SESSION_BUS_ADDRESS $*" >>"$SETUP_LOG"
exit "${STUB_SETUP_STATUS:-0}"
STUB
fingerprint=$(sed -n 's/^SIGNING_KEY_FINGERPRINT=//p' install.sh)
printf '#!/bin/sh\necho "fpr:::::::::%s:"\n' "$fingerprint" >"$work/bin/gpg"
chmod 755 "$work/bin/"*
export PATH="$work/bin:$PATH" PACMAN_LOG="$work/pacman.log" SETUP_LOG="$work/setup.log" SUDO_USER=root

# A fresh scratch /etc, /root, /run/user and home for each scenario.
setup() {
  local s="$work/scene"
  umount -q /etc/pacman.conf /etc/pacman.d /root /sys/firmware 2>/dev/null || true
  rm -rf "${s:?}" "${work:?}/home" "${work:?}/run-user/4242" "$PACMAN_LOG" "$SETUP_LOG"
  mkdir -p "$s/pacman.d" "$s/root/.config/omarchy/hooks/pre-refresh-pacman.d" "$alice_home"
  printf '[options]\nArchitecture = auto\n\n[core]\nInclude = /etc/pacman.d/mirrorlist\n' >"$s/pacman.conf"
  mount --bind "$s/pacman.conf" /etc/pacman.conf
  mount --bind "$s/pacman.d" /etc/pacman.d
  mount --bind "$s/root" /root
  touch "$SETUP_LOG"
}
# Usage: firmware <device-tree model, or empty for no device tree>
firmware() {
  mount -t tmpfs tmpfs /sys/firmware
  if [[ -n $1 ]]; then
    mkdir -p /sys/firmware/devicetree/base
    printf '%s\0' "$1" >/sys/firmware/devicetree/base/model
  fi
}
# A session bus socket for alice, as systemd --user has once she logs in.
session() {
  mkdir -p "$work/run-user/4242"
  python3 -c 'import socket, sys; socket.socket(socket.AF_UNIX).bind(sys.argv[1])' "$work/run-user/4242/bus"
}
hooks=/root/.config/omarchy/hooks/pre-refresh-pacman.d
# What the last install asked for, whichever way it installed (omarchy-pkg-add on Omarchy).
installed() { grep -E '^(pacman -Syu|omarchy-pkg-add) ' "$PACMAN_LOG" | tail -1 | sed -E 's/^(pacman -Syu --needed --noconfirm|omarchy-pkg-add) //'; }
count_include() { grep -cxF "Include = /etc/pacman.d/$1.conf" /etc/pacman.conf || true; }

setup
bash install.sh >"$work/out" 2>&1
check "install.sh installs recall-for-omarchy" [ "$(installed)" = "recall-for-omarchy" ]
check "the repository config is written" grep -qx 'Server = https://github.com/ferdousbhai/recall-for-omarchy/releases/latest/download' /etc/pacman.d/recall-for-omarchy.conf
check "the repository requires signatures" grep -qx 'SigLevel = Required DatabaseRequired' /etc/pacman.d/recall-for-omarchy.conf
check "the Include line is added" [ "$(count_include recall-for-omarchy)" = 1 ]
check "the Omarchy hook is installed" [ -x "$hooks/recall-for-omarchy" ]
check "as root with no desktop user, setup is left to the user" grep -q 'as your desktop user: recall setup' "$work/out"
check "as root with no desktop user, nothing runs setup" [ ! -s "$SETUP_LOG" ]
check "updates come through omarchy update" grep -q 'omarchy update' "$work/out"
bash install.sh --yes >/dev/null 2>&1
check "re-running (with the old --yes) keeps one Include line" [ "$(count_include recall-for-omarchy)" = 1 ]
check "on Omarchy it never runs pacman -Syu (Omarchy's update guard aborts it)" not grep -q '^pacman -Syu' "$PACMAN_LOG"

setup
status=0
bash install.sh --bogus >/dev/null 2>&1 || status=$?
check "an unknown option exits 2" [ "$status" = 2 ]
check "an unknown option changes nothing" [ ! -e /etc/pacman.d/recall-for-omarchy.conf ]

# ARM's update helper removes the old [omarchy] block before appending
# a restricted replacement. A trailing Include must survive that rewrite.
setup
printf '\n[omarchy]\nServer = old\nInclude = /etc/pacman.d/recall-for-omarchy.conf\n' >> /etc/pacman.conf
bash install.sh >/dev/null 2>&1
awk '/^[[:space:]]*\[/ { omit = ($0 ~ /^[[:space:]]*\[omarchy\][[:space:]]*(#.*)?$/) } !omit { print }' /etc/pacman.conf > "$work/arm.conf"
check "repository Include survives ARM source replacement" grep -qxF 'Include = /etc/pacman.d/recall-for-omarchy.conf' "$work/arm.conf"
check "moving an existing Include keeps one copy" [ "$(count_include recall-for-omarchy)" = 1 ]
printf '[options]\n[omarchy]\nServer = old\n' > /etc/pacman.conf
bash "$hooks/recall-for-omarchy"
awk '/^[[:space:]]*\[/ { omit = ($0 ~ /^[[:space:]]*\[omarchy\][[:space:]]*(#.*)?$/) } !omit { print }' /etc/pacman.conf > "$work/arm.conf"
check "refresh hook Include survives ARM source replacement" grep -qxF 'Include = /etc/pacman.d/recall-for-omarchy.conf' "$work/arm.conf"

# aarch64: its own repository; the Asahi Vulkan driver only on Apple hardware.
setup
firmware 'Apple MacBook Pro (16-inch, M1 Max, 2021)'
STUB_ARCH=aarch64 bash install.sh >/dev/null 2>&1
check "aarch64 uses the [recall-for-omarchy-aarch64] repository" [ "$(count_include recall-for-omarchy-aarch64)" = 1 ]
check "aarch64 repository config is written" [ -e /etc/pacman.d/recall-for-omarchy-aarch64.conf ]
check "aarch64 leaves the x86 repository out" [ "$(count_include recall-for-omarchy)" = 0 ]
check "Apple hardware also gets vulkan-asahi" [ "$(installed)" = "recall-for-omarchy vulkan-asahi" ]
setup
firmware 'Raspberry Pi 5 Model B Rev 1.0'
STUB_ARCH=aarch64 bash install.sh >/dev/null 2>&1
check "other ARM hardware does not get vulkan-asahi" [ "$(installed)" = "recall-for-omarchy" ]
setup
firmware ''
STUB_ARCH=aarch64 bash install.sh >/dev/null 2>&1
check "ARM without a device tree does not get vulkan-asahi" [ "$(installed)" = "recall-for-omarchy" ]
setup
firmware 'Apple MacBook Pro'
bash install.sh >/dev/null 2>&1
check "x86_64 never gets vulkan-asahi" [ "$(installed)" = "recall-for-omarchy" ]

setup
status=0
STUB_ARCH=riscv64 bash install.sh >/dev/null 2>&1 || status=$?
check "an unsupported architecture fails" [ "$status" = 1 ]
check "an unsupported architecture changes nothing" [ ! -e /etc/pacman.d/recall-for-omarchy.conf ]

# Plain Arch: no Omarchy commands anywhere on PATH, so one pacman -Syu transaction.
setup
mkdir -p "$work/plain"
cp "$work/bin/pacman" "$work/bin/pacman-key" "$work/bin/curl" "$work/bin/gpg" "$work/bin/uname" "$work/plain/"
for tool in bash sh sed grep mktemp rm tee id getent cut install chown chmod cat env dirname basename tr sort head tail awk cmp; do
  ln -sf "$(command -v "$tool")" "$work/plain/$tool"
done
PATH="$work/plain" bash install.sh >"$work/plain.out" 2>&1 || true
check "without Omarchy it installs in one pacman -Syu" grep -qx 'pacman -Syu --needed --noconfirm recall-for-omarchy' "$PACMAN_LOG"

# The desktop user who ran sudo: setup runs as them, in their systemd session.
setup
session
SUDO_USER=alice bash install.sh >"$work/out" 2>&1
check "setup runs as the desktop user" grep -qx 'runuser -u alice -- env XDG_RUNTIME_DIR=/run/user/4242 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/4242/bus systemd-run --user --wait --pipe --collect --quiet /usr/bin/recall setup' "$SETUP_LOG"
check "setup reaches the user's session bus" grep -q '^systemd-run XDG_RUNTIME_DIR=/run/user/4242 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/4242/bus ' "$SETUP_LOG"
check "setup keeps the keybinding by default" not grep -q RECALL_NO_KEYBINDING "$SETUP_LOG"
check "the installer says it is done" grep -q '^Done' "$work/out"
setup
session
SUDO_USER=alice RECALL_NO_KEYBINDING=1 bash install.sh >/dev/null 2>&1
check "RECALL_NO_KEYBINDING=1 reaches setup" grep -q -- '--setenv=RECALL_NO_KEYBINDING=1 /usr/bin/recall setup$' "$SETUP_LOG"

# A tarball install from an earlier installer, and alice is not logged in:
# the package installs, and her old copy keeps working until she runs setup.
setup
old="$alice_home/.local/share/recall-for-omarchy/releases/v0.4.3/recall-for-omarchy/target/release"
mkdir -p "$old" "$alice_home/.local/bin" "$alice_home/.config/recall" "$alice_home/.local/share/recall/index"
printf '#!/bin/sh\nexec %q "$@"\n' "$old/recall" >"$alice_home/.local/bin/recall"
echo old >"$old/recall"
echo '{"roots":[]}' >"$alice_home/.config/recall/settings.json"
echo index >"$alice_home/.local/share/recall/index/index.sqlite"
find "$alice_home" -type f -exec sha256sum {} + | sort >"$work/before"
status=0
SUDO_USER=alice bash install.sh >"$work/out" 2>&1 || status=$?
check "migration: without a session the install still succeeds" [ "$status" = 0 ]
check "migration: without a session it asks for setup after login" grep -q 'After logging in, run: recall setup' "$work/out"
check "migration: without a session nothing runs setup" [ ! -s "$SETUP_LOG" ]
check "migration: the installer itself leaves the old install, settings and index alone" \
  [ "$(find "$alice_home" -type f -exec sha256sum {} + | sort)" = "$(cat "$work/before")" ]
session
SUDO_USER=alice bash install.sh >/dev/null 2>&1
check "migration: with a session setup takes over" grep -q '/usr/bin/recall setup$' "$SETUP_LOG"

setup
session
status=0
SUDO_USER=alice STUB_SETUP_STATUS=1 bash install.sh >"$work/out" 2>&1 || status=$?
check "a failed setup fails the installer" [ "$status" = 1 ]
check "a failed setup says how to retry" grep -q 'Run it again as alice: recall setup' "$work/out"
check "a failed setup still installed the package" [ "$(installed)" = "recall-for-omarchy" ]

umount -q /etc/pacman.conf /etc/pacman.d /root /sys/firmware 2>/dev/null || true
if (( fail )); then
  echo "FAIL install_test" >&2
  exit 1
fi
echo "ok install_test"
