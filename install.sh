#!/bin/bash
# Install Recall for Omarchy (or any Arch Linux) from its signed package
# repository, keep it updating with the system, and set it up for you:
#
#   curl -fsSL https://ferdousbhai.com/recall/install.sh | sudo bash
#
# Every step is idempotent, so re-running is safe. It trusts the
# package-signing key (checked against the fingerprint pinned below), adds
# the [recall-for-omarchy] repository, installs an Omarchy hook that restores
# it after `omarchy refresh pacman` rewrites /etc/pacman.conf, and installs
# the package (with vulkan-asahi on Apple hardware). Then, as the desktop user
# who ran sudo and in that user's own session, it runs `recall setup`: the
# background service, the search panel, the Super+Ctrl+Shift+F block in
# ~/.config/hypr/bindings.lua (backed up first; `| sudo RECALL_NO_KEYBINDING=1 bash`
# skips it), the user launchers and the agent skill. That also takes over an
# install made by earlier, tarball-based installers; settings, indexes and
# models are kept. A first setup selects ~/Documents and ~/Pictures (when they
# exist); the embedding models download once there are folders to index.
set -euo pipefail

REPO=recall-for-omarchy
RELEASES=${RECALL_RELEASES:-https://github.com/ferdousbhai/recall-for-omarchy/releases/latest/download}
SIGNING_KEY_FINGERPRINT=52130299581DAD685226900CA89CA1A6A1E74251
PACKAGE=recall-for-omarchy

# --- add_signed_repo (shared) ---
# Trust a project's package-signing key (checked against the pinned
# fingerprint), add its signed pacman repository, and keep the repository
# across `omarchy refresh pacman`, which rewrites /etc/pacman.conf from
# Omarchy's defaults and then runs the user's pre-refresh-pacman hooks.
# Works as root (`sudo bash`) or as a desktop user (sudo inside). This text
# is identical in every installer that uses it, and each repository's test
# pins its hash: change it here and in its twins together.
add_signed_repo() {
  local name="$1" release="$2" fingerprint="$3"
  local conf="/etc/pacman.d/$name.conf" include="Include = /etc/pacman.d/$name.conf"
  local sudo='' key user home hook_dir
  (( EUID == 0 )) || sudo=sudo
  key="$(mktemp)"
  if ! curl -fsSL "$release/$name-signing-key.asc" -o "$key"; then
    rm -f "$key"
    echo "Could not download the package-signing key from $release." >&2
    return 1
  fi
  if ! gpg --batch --with-colons --show-keys "$key" 2>/dev/null | grep -q "^fpr:*:$fingerprint:"; then
    rm -f "$key"
    echo "The downloaded key does not match the pinned fingerprint $fingerprint; nothing was changed." >&2
    return 1
  fi
  $sudo pacman-key --add "$key"
  $sudo pacman-key --lsign-key "$fingerprint"
  rm -f "$key"
  printf '[%s]\nSigLevel = Required DatabaseRequired\nServer = %s\n' "$name" "$release" | $sudo tee "$conf" >/dev/null
  # ARM updates replace [omarchy] and everything below its header up to
  # the next section. Keep our Include before repository sections so it
  # cannot be mistaken for part of [omarchy], even on an existing install.
  restore_repo_include() {
    local include=$1 temporary sudo=''
    (( EUID == 0 )) || sudo=sudo
    temporary=$(mktemp)
    awk -v repo_include="$include" '
      $0 == repo_include { next }
      !added && /^[[:space:]]*\[/ && !/^[[:space:]]*\[options\]/ {
        print repo_include
        added = 1
      }
      { print }
      END { if (!added) print repo_include }
    ' /etc/pacman.conf > "$temporary"
    if ! cmp -s "$temporary" /etc/pacman.conf; then
      $sudo tee /etc/pacman.conf < "$temporary" >/dev/null
    fi
    rm -f "$temporary"
  }
  restore_repo_include "$include"
  user="${SUDO_USER:-${USER:-$(id -un)}}"
  home="$(getent passwd "$user" | cut -d: -f6)"
  if [[ -n $home && -d $home/.config/omarchy ]]; then
    hook_dir="$home/.config/omarchy/hooks/pre-refresh-pacman.d"
    install -d -o "$user" -g "$(id -gn "$user")" "$hook_dir"
    {
      printf '%s\n' '#!/bin/bash' 'set -euo pipefail' \
        "# Restore the [$name] repository before Omarchy's ARM source rewrite."
      declare -f restore_repo_include
      printf 'restore_repo_include %q\n' "$include"
    } > "$hook_dir/$name"
    chown "$user" "$hook_dir/$name"
    chmod 755 "$hook_dir/$name"
  fi
  $sudo pacman -Sy
}
# --- end add_signed_repo ---

for arg in "$@"; do
  case $arg in
    -y|--yes) ;; # Earlier installers asked before installing packages; this one never asks.
    *) echo "Unknown option: $arg" >&2; exit 2 ;;
  esac
done
if ! command -v pacman >/dev/null; then
  echo "pacman not found: this installer is for Omarchy and other Arch Linux systems." >&2
  exit 1
fi
if [[ ! $SIGNING_KEY_FINGERPRINT =~ ^[0-9A-F]{40}$ ]]; then
  echo "This copy of install.sh has no signing key pinned; nothing was changed." >&2
  exit 1
fi

packages=("$PACKAGE")
# One repository per architecture: ARM needs a separate database.
case "$(uname -m)" in
  x86_64) ;;
  aarch64)
    REPO+=-aarch64
    # Apple GPUs on Linux use the Asahi Vulkan userspace driver; other ARM
    # machines bring their own driver, or Recall uses the CPU.
    if grep -aqs Apple /sys/firmware/devicetree/base/model; then
      packages+=(vulkan-asahi)
    fi
    ;;
  *) echo "Unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

echo "Adding the [$REPO] repository"
add_signed_repo "$REPO" "$RELEASES" "$SIGNING_KEY_FINGERPRINT"

echo "Installing ${packages[*]}"
if command -v omarchy-pkg-add >/dev/null; then
  # Omarchy's pacman hook aborts a direct `pacman -Syu` (system upgrades go
  # through `omarchy update`), so install the way Omarchy installs its own
  # apps; the next `omarchy update` brings everything current.
  omarchy-pkg-add "${packages[@]}"
else
  # Upgrade and install in one transaction. add_signed_repo has just synced
  # every repository's database, and installing from those without upgrading
  # is Arch's unsupported partial upgrade.
  if (( EUID == 0 )); then
    pacman -Syu --needed --noconfirm "${packages[@]}"
  else
    sudo pacman -Syu --needed --noconfirm "${packages[@]}"
  fi
fi

# `recall setup` writes only the desktop user's own files, so it runs as that
# user (not root), through their systemd session: that session has the
# Hyprland and Omarchy environment the panel and keybinding need, which sudo
# does not pass on.
desktop_setup() {
  local user uid
  user="${SUDO_USER:-${USER:-$(id -un)}}"
  uid="$(id -u "$user" 2>/dev/null)" || uid=''
  if [[ -z $uid || $uid == 0 ]]; then
    echo 'Then set it up as your desktop user: recall setup'
    return 0
  fi
  if [[ ! -S /run/user/$uid/bus ]]; then
    echo "No desktop session for $user right now. After logging in, run: recall setup"
    return 0
  fi
  local run=(env "XDG_RUNTIME_DIR=/run/user/$uid" "DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/$uid/bus"
    systemd-run --user --wait --pipe --collect --quiet)
  [[ -z ${RECALL_NO_KEYBINDING:-} ]] || run+=("--setenv=RECALL_NO_KEYBINDING=$RECALL_NO_KEYBINDING")
  (( EUID == 0 )) && run=(runuser -u "$user" -- "${run[@]}")
  echo "Setting up Recall for $user"
  if ! "${run[@]}" /usr/bin/recall setup </dev/null; then
    echo "Recall is installed, but its desktop setup failed. Run it again as $user: recall setup" >&2
    return 1
  fi
}
desktop_setup

cat <<EOT

Done. Open search with Super+Ctrl+Shift+F; Settings chooses the folders to
index. The embedding models (about 865 MB) download once there are folders.
Agents: recall skill
Updates arrive with the rest of the system through: omarchy update
EOT
