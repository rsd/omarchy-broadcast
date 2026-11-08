#!/usr/bin/env bash
# install-user.sh — install oma-broadcast from this checkout for the current
# user, without root.
#
# Builds the release binaries and installs:
#   oma-broadcastd, oma-broadcast  -> ${XDG_BIN_HOME:-~/.local/bin}/
#   systemd user unit              -> ${XDG_DATA_HOME:-~/.local/share}/systemd/user/oma-broadcastd.service
#                                     (from dist/, ExecStart pointing at the installed daemon)
#   D-Bus activation file          -> ${XDG_DATA_HOME:-~/.local/share}/dbus-1/services/br.com.dias.OmaBroadcast.service
#   install manifest               -> ${XDG_STATE_HOME:-~/.local/state}/oma-broadcast/install-user.manifest
#
# It runs no systemctl command and enables nothing; it prints the next steps.
# It does not install the WirePlumber drop-in or the Omarchy bar plugin.
#
# Usage:
#   scripts/install-user.sh [options]
#
# Options:
#   --dry-run    Print every action; build nothing, change nothing.
#   --no-build   Install the binaries already in ${CARGO_TARGET_DIR:-target}/release
#                instead of running cargo build.
#   --uninstall  Remove what this script installed, and only that.
#   -h, --help   Show this help.
#
# Ownership: every text file it writes carries a marker comment line, and the
# manifest lists every path it wrote, with the sha256 of each binary. It
# overwrites or removes a text file only if it carries the marker, and a
# binary only if the manifest lists it with the checksum of the file on disk.
# Any other file at a target path stops it before anything is changed.
# Re-running it over its own install updates that install.
#
# Exit status: 0 done; 1 refused (a target path holds a file this script did
# not install, or the unit is still enabled on --uninstall) or build failed;
# 2 usage error or unusable environment.
set -euo pipefail

MARKER='# Installed by oma-broadcast scripts/install-user.sh; remove with scripts/install-user.sh --uninstall'
BINARIES=(oma-broadcastd oma-broadcast)
UNIT_NAME=oma-broadcastd.service
DBUS_NAME=br.com.dias.OmaBroadcast.service

usage() {
  sed -n '2,/^set -euo pipefail$/{/^set -euo/d;s/^# \{0,1\}//;p}' "$0"
}

die() {
  printf 'install-user: %s\n' "$*" >&2
  exit 2
}

warn() { printf 'install-user: warning: %s\n' "$*" >&2; }

dry_run=0
build=1
mode=install
while (($#)); do
  case $1 in
    --dry-run) dry_run=1; shift ;;
    --no-build) build=0; shift ;;
    --uninstall) mode=uninstall; shift ;;
    -h | --help) usage; exit 0 ;;
    *) die "unknown argument: $1 (see --help)" ;;
  esac
done

if ((EUID == 0)); then
  die "refusing to run as root: this is a per-user install into your home directory; run it as your own user"
fi
[[ ${HOME:-} == /* ]] || die "HOME is unset or not an absolute path"

# XDG base directories: a relative value is invalid and ignored (XDG Base
# Directory specification).
xdg_dir() {
  local name=$1 fallback=$2 value=${!1:-}
  if [[ -n $value && $value != /* ]]; then
    warn "$name=$value is not an absolute path; using $fallback"
    value=""
  fi
  printf '%s\n' "${value:-$fallback}"
}

# CDPATH='': with CDPATH exported, cd would print the path it resolved.
repo=$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." >/dev/null && pwd)
bin_dir=$(xdg_dir XDG_BIN_HOME "$HOME/.local/bin")
data_dir=$(xdg_dir XDG_DATA_HOME "$HOME/.local/share")
state_dir=$(xdg_dir XDG_STATE_HOME "$HOME/.local/state")/oma-broadcast
config_dir=$(xdg_dir XDG_CONFIG_HOME "$HOME/.config")
target_dir=${CARGO_TARGET_DIR:-$repo/target}

unit_src=$repo/dist/systemd/$UNIT_NAME
dbus_src=$repo/dist/dbus-1/services/$DBUS_NAME
unit_dst=$data_dir/systemd/user/$UNIT_NAME
dbus_dst=$data_dir/dbus-1/services/$DBUS_NAME
manifest=$state_dir/install-user.manifest

bin_dsts=()
for b in "${BINARIES[@]}"; do bin_dsts+=("$bin_dir/$b"); done

# The bin dir is written into ExecStart, where systemd expands % specifiers,
# $ variables and quoting; accept only paths that need none of that.
[[ $bin_dir =~ ^/[A-Za-z0-9._/@+-]*$ ]] ||
  die "bin dir '$bin_dir' contains characters that would need escaping in ExecStart; set XDG_BIN_HOME to a plain path"

# Temporary files this run created, removed by exact path on exit, so an
# interrupted run leaves none behind.
tmp_files=()
cleanup() {
  if ((${#tmp_files[@]})); then rm -f -- "${tmp_files[@]}"; fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# Print a command, then run it unless --dry-run.
run() {
  printf '+'
  printf ' %q' "$@"
  printf '\n'
  ((dry_run)) || "$@"
}

is_binary_target() {
  local b
  for b in "${BINARIES[@]}"; do [[ ${1##*/} == "$b" ]] && return 0; done
  return 1
}

# Manifest format, after the marker and comment lines: one path per line for
# a text file, and "<sha256>  <path>" (sha256sum output) for a binary. A path
# may appear with more than one checksum while an install is in progress.
sha_re='^[0-9a-f]{64}  (/.*)$'

# Print the path of a manifest line, or nothing for a comment line.
manifest_path() {
  if [[ $1 =~ $sha_re ]]; then
    printf '%s\n' "${BASH_REMATCH[1]}"
  elif [[ $1 == /* ]]; then
    printf '%s\n' "$1"
  fi
}

# 0 if the manifest lists $1, with or without a checksum.
manifest_lists() {
  local line
  [[ -f $manifest ]] || return 1
  while IFS= read -r line; do
    [[ $(manifest_path "$line") == "$1" ]] && return 0
  done <"$manifest"
  return 1
}

# 0 if the manifest lists $1 with the checksum $2.
manifest_has_sum() {
  [[ -f $manifest ]] && grep -qxF -- "$2  $1" "$manifest"
}

file_sum() {
  local sum
  sum=$(sha256sum -- "$1") || return 1
  printf '%s\n' "${sum%% *}"
}

# Ownership of an existing path: 0 if this script installed it. A text file
# is ours if it carries the marker line, a binary if the manifest lists it
# with the checksum of the file on disk. Sets WHY to the reason when it is
# not ours.
owned() {
  local path=$1 sum
  WHY=""
  if [[ -L $path || ! -f $path ]]; then
    WHY="not a regular file"
    return 1
  fi
  if is_binary_target "$path"; then
    sum=$(file_sum "$path") || {
      WHY="cannot read it to check its sha256"
      return 1
    }
    manifest_has_sum "$path" "$sum" && return 0
    # A listed binary whose checksum differs was replaced since; a bare path
    # without a checksum (a manifest from an older install-user.sh) cannot
    # be verified, so both are refused like any foreign file.
    if grep -qxF -- "$path" "$manifest" 2>/dev/null; then
      WHY="listed in $manifest without a sha256 (an older install-user.sh), so it cannot be verified; check it and remove it by hand, then re-run"
    elif manifest_lists "$path"; then
      WHY="it changed since install-user.sh installed it (its sha256 does not match $manifest); check it and remove it by hand if it is not needed"
    else
      WHY="binary not listed in $manifest"
    fi
    return 1
  fi
  grep -qxF -- "$MARKER" "$path" && return 0
  WHY="no install-user marker line"
  if [[ $path == "$dbus_dst" ]] && cmp -s -- "$path" "$dbus_src"; then
    WHY+="; it is identical to dist/dbus-1/services/$DBUS_NAME (a manual install): remove it and re-run"
  fi
  return 1
}

# Check every path before changing anything; exit 1 listing the foreign ones.
check_targets() {
  local path foreign=0
  for path in "$@"; do
    [[ -e $path || -L $path ]] || continue
    if ! owned "$path"; then
      printf 'install-user: refusing to touch %s: %s\n' "$path" "$WHY" >&2
      foreign=1
    fi
  done
  if ((foreign)); then
    printf 'install-user: nothing was changed\n' >&2
    exit 1
  fi
}

# Install the output of the command "${@:2}" at $1 with mode 644, atomically.
# It runs in this shell, not in a pipeline subshell, so the temporary file is
# registered for the exit trap.
put_text() {
  local dst=$1 tmp
  shift
  if ((dry_run)); then
    printf '+ write %q (mode 644, with the install-user marker)\n' "$dst"
    return
  fi
  mkdir -p -- "$(dirname -- "$dst")"
  tmp=$(mktemp "$dst.XXXXXX")
  tmp_files+=("$tmp")
  "$@" >"$tmp"
  chmod 644 -- "$tmp"
  mv -f -- "$tmp" "$dst"
  printf '+ wrote %q\n' "$dst"
}

# --- uninstall -------------------------------------------------------------

if [[ $mode == uninstall ]]; then
  targets=("${bin_dsts[@]}" "$unit_dst" "$dbus_dst")
  # Also what an earlier run wrote under other XDG directories.
  if [[ -f $manifest ]]; then
    while IFS= read -r line; do
      path=$(manifest_path "$line")
      [[ -n $path ]] && targets+=("$path")
    done <"$manifest"
  fi
  mapfile -t targets < <(printf '%s\n' "${targets[@]}" | sort -u)

  # Removing the file of an enabled unit leaves a dangling enable link:
  # disable it first.
  enabled=0
  for link in "$config_dir"/systemd/user/*.wants/"$UNIT_NAME"; do
    [[ -L $link ]] || continue
    for path in "${targets[@]}"; do
      if [[ ${path##*/} == "$UNIT_NAME" && $(readlink -- "$link") == "$path" ]]; then
        printf 'install-user: %s is still enabled (%s)\n' "$UNIT_NAME" "$link" >&2
        enabled=1
      fi
    done
  done
  if ((enabled)); then
    printf 'install-user: run this first, then re-run --uninstall:\n  systemctl --user disable --now oma-broadcastd\n' >&2
    exit 1
  fi

  check_targets "${targets[@]}" "$manifest"

  removed=0
  for path in "${targets[@]}"; do
    [[ -e $path ]] || continue
    run rm -f -- "$path"
    removed=1
  done
  if [[ -e $manifest ]]; then
    run rm -f -- "$manifest"
    run rmdir --ignore-fail-on-non-empty -- "$state_dir"
    removed=1
  fi
  if ((!removed)); then
    echo "Nothing installed by install-user.sh was found."
    exit 0
  fi
  done_msg="Uninstalled."
  ((!dry_run)) || done_msg="Dry run: nothing was changed."
  cat <<EOF

$done_msg
Next steps (not run by this script):
  systemctl --user stop oma-broadcastd    # if it is still running
  systemctl --user daemon-reload
EOF
  exit 0
fi

# --- install ---------------------------------------------------------------

[[ -f $unit_src ]] || die "missing $unit_src"
[[ -f $dbus_src ]] || die "missing $dbus_src"

# The shipped unit must have exactly one ExecStart naming the daemon, whatever
# its path (/usr/bin for the package, %h/.cargo/bin in older checkouts).
exec_re='^ExecStart=.*oma-broadcastd$'
n=$(grep -c -- "$exec_re" "$unit_src" || true)
((n == 1)) || {
  printf 'install-user: expected exactly one line matching %s in %s, found %s\n' "$exec_re" "$unit_src" "$n" >&2
  exit 2
}

# Previously installed paths that still exist stay in the manifest, so
# --uninstall finds them even if the XDG directories changed since. The
# current targets are left out here and listed afresh below.
kept=()
if [[ -f $manifest ]]; then
  while IFS= read -r line; do
    path=$(manifest_path "$line")
    [[ -n $path && -e $path ]] || continue
    case $path in
      "$unit_dst" | "$dbus_dst") continue ;;
    esac
    for dst in "${bin_dsts[@]}"; do [[ $path == "$dst" ]] && continue 2; done
    kept+=("$line")
  done <"$manifest"
fi

check_targets "${bin_dsts[@]}" "$unit_dst" "$dbus_dst" "$manifest"

if ((build)); then
  run cargo build --manifest-path "$repo/Cargo.toml" --release --locked -p oma-broadcastd -p oma-broadcast || {
    printf 'install-user: cargo build failed; nothing was changed\n' >&2
    exit 1
  }
fi
if ((!dry_run)); then
  for b in "${BINARIES[@]}"; do
    [[ -x $target_dir/release/$b ]] || die "no built binary at $target_dir/release/$b (run without --no-build)"
  done
fi

# The manifest text: its marker, the kept lines and the given lines.
manifest_text() {
  echo "$MARKER"
  echo "# Paths written by install-user.sh, one per line; a binary as \"<sha256>  <path>\"."
  printf '%s\n' "${kept[@]}" "$@" "$unit_dst" "$dbus_dst" | sort -u
}

run mkdir -p -- "$bin_dir"
# Install beside the target, then rename: replacing a running binary in place
# would fail with "Text file busy".
old_sums=() new_sums=()
for b in "${BINARIES[@]}"; do
  tmp=$bin_dir/.$b.install-user
  ((dry_run)) || tmp_files+=("$tmp")
  run install -m755 -T -- "$target_dir/release/$b" "$tmp"
  if ((dry_run)); then
    new_sums+=("$bin_dir/$b")
  else
    new_sums+=("$(file_sum "$tmp")  $bin_dir/$b")
    # check_targets verified any binary already there against the manifest.
    [[ ! -e $bin_dir/$b ]] || old_sums+=("$(file_sum "$bin_dir/$b")  $bin_dir/$b")
  fi
done

# The manifest lists both checksums while the binaries are replaced, so an
# interrupted run leaves either version recognised and a re-run may proceed.
put_text "$manifest" manifest_text "${old_sums[@]}" "${new_sums[@]}"
for b in "${BINARIES[@]}"; do
  run mv -f -T -- "$bin_dir/.$b.install-user" "$bin_dir/$b"
done
put_text "$manifest" manifest_text "${new_sums[@]}"

unit_text() {
  echo "$MARKER"
  echo "# Generated from dist/systemd/$UNIT_NAME; ExecStart points at the installed binary."
  sed "s|$exec_re|ExecStart=$bin_dir/oma-broadcastd|" "$unit_src"
}
put_text "$unit_dst" unit_text

dbus_text() {
  echo "$MARKER"
  cat "$dbus_src"
}
put_text "$dbus_dst" dbus_text

if [[ -e $config_dir/systemd/user/$UNIT_NAME ]]; then
  warn "$config_dir/systemd/user/$UNIT_NAME exists and takes precedence over $unit_dst, which it shadows; to use the new unit: systemctl --user disable oma-broadcastd, then remove that file"
fi
case ":${PATH:-}:" in
  *":$bin_dir:"*) ;;
  *) warn "$bin_dir is not on PATH; the oma-broadcast CLI must be called by its full path" ;;
esac

plugin=$repo/clients/omarchy-bar/br.com.dias.oma-broadcast
wp_src=$repo/dist/systemd/wireplumber.service.d/oma-broadcast.conf
done_msg="Installed."
((!dry_run)) || done_msg="Dry run: nothing was changed."
cat <<EOF

$done_msg
Next steps (not run by this script):
  systemctl --user daemon-reload
  systemctl --user enable --now oma-broadcastd
  (already running: systemctl --user restart oma-broadcastd)

Optional, start WirePlumber after the daemon so PipeWire lists the virtual
camera (see $(printf '%q' "$wp_src")):
  install -Dm644 $(printf '%q' "$wp_src") $(printf '%q' "$config_dir/systemd/user/wireplumber.service.d/oma-broadcast.conf")
  systemctl --user daemon-reload

Optional, the Omarchy bar plugin:
  mkdir -p ~/.config/omarchy/plugins && ln -s $(printf '%q' "$plugin") ~/.config/omarchy/plugins/br.com.dias.oma-broadcast
  omarchy plugin enable br.com.dias.oma-broadcast
EOF
