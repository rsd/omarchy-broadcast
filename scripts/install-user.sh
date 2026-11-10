#!/usr/bin/env bash
# install-user.sh — install omarchy-broadcast from this checkout for the current
# user, without root.
#
# Builds the release binaries and the NVIDIA Maxine module, and installs:
#   omarchy-broadcastd, omarchy-broadcast -> ${XDG_BIN_HOME:-~/.local/bin}/
#   systemd user unit                     -> ${XDG_DATA_HOME:-~/.local/share}/systemd/user/omarchy-broadcastd.service
#                                            (from dist/, ExecStart pointing at the installed daemon)
#   NVIDIA Maxine module library          -> ${XDG_DATA_HOME:-~/.local/share}/omarchy-broadcast/modules/libobc_module_maxine.so
#   its module manifest                   -> ${XDG_DATA_HOME:-~/.local/share}/omarchy-broadcast/modules.d/maxine-vfx.toml
#                                            (from crates/obc-module-maxine/maxine-vfx.toml, library pointing at the
#                                            installed module library)
#   D-Bus activation file                 -> ${XDG_DATA_HOME:-~/.local/share}/dbus-1/services/br.com.dias.OmarchyBroadcast.service
#   install manifest                      -> ${XDG_STATE_HOME:-~/.local/state}/omarchy-broadcast/install-user.manifest
# and, with --mic, the noise-removed microphone:
#   its PipeWire configurations           -> ${XDG_DATA_HOME:-~/.local/share}/omarchy-broadcast/pipewire/
#                                            (omarchy-broadcast-mic-rnnoise.conf and
#                                            omarchy-broadcast-mic-deepfilter.conf, from dist/pipewire/)
#   its systemd user unit template        -> ${XDG_DATA_HOME:-~/.local/share}/systemd/user/omarchy-broadcast-mic@.service
#                                            (from dist/, ExecStart pointing at the installed configurations)
#
# The Maxine module is always built and installed (the library builds without
# the Maxine SDK). Without the SDK the daemon lists the module greyed out, with
# the reason, so there is no flag for it. The SDK and the os-release shim it
# needs still come from the maxine-vfx-sdk-local package (built from the
# omarchy-nvidia-sdks repository), and the Maxine drop-in that uses them is a
# manual step, printed at the end (docs/setup/maxine.md, section 3). The
# daemon loads a module library once and never reloads it: restart a running
# daemon to pick up a rebuilt module. An earlier version of this script
# installed its own copy of the shim in ~/.local/lib/omarchy-broadcast; a
# re-run removes that copy (see Ownership).
#
# It runs no systemctl command and enables nothing; it prints the next steps.
# It does not install the WirePlumber drop-in, the Maxine drop-in or, without
# --mic, the microphone. The Omarchy bar plugin is a separate project.
#
# Usage:
#   scripts/install-user.sh [options]
#
# Options:
#   --dry-run    Print every action; build nothing, change nothing.
#   --no-build   Build nothing: install the binaries and the module library
#                already in ${CARGO_TARGET_DIR:-target}/release.
#   --mic        Also install the noise-removed microphone (see above). Without
#                it, microphone files installed by an earlier run stay in
#                place and in the manifest, so --uninstall still removes them.
#   --uninstall  Remove what this script installed, and only that, including
#                the Maxine module and its manifest, the microphone's files,
#                and the directories ~/.local/share/omarchy-broadcast/modules,
#                modules.d and pipewire and ~/.local/share/omarchy-broadcast
#                once they are empty (modules and modules.d even when no file
#                of theirs was installed), and the shim an earlier version
#                installed (with ~/.local/lib/omarchy-broadcast once it is
#                empty).
#   -h, --help   Show this help.
#
# Ownership: every text file it writes carries a marker comment line, and the
# manifest lists every path it wrote, with the sha256 of each binary. It
# overwrites or removes a text file only if it carries the marker, and a
# binary (the daemon, the CLI and the module library) only if the manifest
# lists it with the checksum of the file on disk.
# Any other file at a target path stops it before anything is changed.
# Re-running it over its own install updates that install. The shim files of
# an earlier version (os-release-shim.so and fake-os-release, listed in its
# manifest with their sha256) follow the binaries' rule: a re-run or
# --uninstall removes them when the checksum matches. One that changed since
# is left in place and listed: a re-run warns about it, and --uninstall
# refuses it like a changed binary.
#
# Exit status: 0 done; 1 refused (a target path holds a file this script did
# not install, or a unit is still enabled on --uninstall) or build failed;
# 2 usage error or unusable environment.
set -euo pipefail

MARKER='# Installed by omarchy-broadcast scripts/install-user.sh; remove with scripts/install-user.sh --uninstall'
BINARIES=(omarchy-broadcastd omarchy-broadcast)
# The os-release shim files an earlier version of this script installed in
# ~/.local/lib/omarchy-broadcast. It installs them no more; they are only
# removed, through the manifest of that version.
LEGACY_SHIM=(os-release-shim.so fake-os-release)
# The Maxine module: a cdylib built from crate obc-module-maxine, and its
# manifest (a text file, generated from the one in that crate).
MODULE_CRATE=obc-module-maxine
MODULE_LIB=libobc_module_maxine.so
MODULE_MANIFEST=maxine-vfx.toml
# File names whose ownership is the manifest checksum, not a marker line.
SUMMED=("${BINARIES[@]}" "$MODULE_LIB" "${LEGACY_SHIM[@]}")
UNIT_NAME=omarchy-broadcastd.service
DBUS_NAME=br.com.dias.OmarchyBroadcast.service
MIC_UNIT_NAME=omarchy-broadcast-mic@.service
MIC_CONFS=(omarchy-broadcast-mic-rnnoise.conf omarchy-broadcast-mic-deepfilter.conf)

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
want_mic=0
mode=install
while (($#)); do
  case $1 in
    --dry-run) dry_run=1; shift ;;
    --no-build) build=0; shift ;;
    --mic) want_mic=1; shift ;;
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
state_dir=$(xdg_dir XDG_STATE_HOME "$HOME/.local/state")/omarchy-broadcast
config_dir=$(xdg_dir XDG_CONFIG_HOME "$HOME/.config")
# Absolute, as cargo resolves a relative CARGO_TARGET_DIR (against the cwd).
target_dir=$(realpath -m -- "${CARGO_TARGET_DIR:-$repo/target}")

unit_src=$repo/dist/systemd/$UNIT_NAME
dbus_src=$repo/dist/dbus-1/services/$DBUS_NAME
unit_dst=$data_dir/systemd/user/$UNIT_NAME
dbus_dst=$data_dir/dbus-1/services/$DBUS_NAME
manifest=$state_dir/install-user.manifest
mic_unit_src=$repo/dist/systemd/$MIC_UNIT_NAME
mic_unit_dst=$data_dir/systemd/user/$MIC_UNIT_NAME
mic_conf_dir=$data_dir/omarchy-broadcast/pipewire
module_manifest_src=$repo/crates/$MODULE_CRATE/$MODULE_MANIFEST
module_dir=$data_dir/omarchy-broadcast/modules
module_lib_dst=$module_dir/$MODULE_LIB
module_manifest_dst=$data_dir/omarchy-broadcast/modules.d/$MODULE_MANIFEST
maxine_src=$repo/dist/systemd/omarchy-broadcastd.service.d/maxine.conf
# The user's Maxine drop-in, if any: only read, never written.
maxine_dropin=$config_dir/systemd/user/omarchy-broadcastd.service.d/maxine.conf

bin_dsts=()
for b in "${BINARIES[@]}"; do bin_dsts+=("$bin_dir/$b"); done
# The microphone files this run installs: only with --mic.
mic_dsts=()
if ((want_mic)); then
  mic_dsts=("$mic_unit_dst")
  for c in "${MIC_CONFS[@]}"; do mic_dsts+=("$mic_conf_dir/$c"); done
fi

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
  for b in "${SUMMED[@]}"; do [[ ${1##*/} == "$b" ]] && return 0; done
  return 1
}

# 0 if $1 is a shim file of an earlier version (by file name, as the
# manifest lists it).
is_legacy_shim() {
  local b
  for b in "${LEGACY_SHIM[@]}"; do [[ ${1##*/} == "$b" ]] && return 0; done
  return 1
}

# Print the note for the shim of an earlier version, after "$1" (the
# sentence that says it was, or with --dry-run would be, removed). A Maxine
# drop-in that preloads it from ~/.local/lib/omarchy-broadcast gets a note
# naming it; the drop-in itself is only read, never changed.
legacy_shim_note() {
  local intro=$1
  if [[ -f $maxine_dropin ]] && grep -qF -- .local/lib/omarchy-broadcast "$maxine_dropin" 2>/dev/null; then
    cat <<EOF

$intro
$(printf '%q' "$maxine_dropin")
preloads that shim from ~/.local/lib/omarchy-broadcast, so it no longer
works. Replace it with the current
dist/systemd/omarchy-broadcastd.service.d/maxine.conf after installing the
maxine-vfx-sdk-local package (docs/setup/maxine.md, section 3):
  install -Dm644 $(printf '%q' "$maxine_src") $(printf '%q' "$maxine_dropin")
or remove it:
  rm $(printf '%q' "$maxine_dropin")
then reload:
  systemctl --user daemon-reload
EOF
  else
    cat <<EOF

$intro
A Maxine drop-in that still points at it is out of date: replace it with the
current dist/systemd/omarchy-broadcastd.service.d/maxine.conf, which uses the
maxine-vfx-sdk-local package (docs/setup/maxine.md, section 3), or remove it.
EOF
  fi
}

# Manifest format, after the marker and comment lines: one path per line for
# a text file, and "<sha256>  <path>" (sha256sum output) for a binary (and,
# in the manifest of an earlier version, the shim and fake-os-release). A
# path may appear with more than one checksum while an install is in
# progress.
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
# is ours if it carries the marker line, a binary (or a legacy shim file) if
# the manifest lists it with the checksum of the file on disk. Sets WHY to
# the reason when it is not ours.
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
      WHY="not listed in $manifest (not installed by install-user.sh)"
    fi
    return 1
  fi
  grep -qxF -- "$MARKER" "$path" && return 0
  WHY="no install-user marker line"
  if [[ $path == "$dbus_dst" ]] && cmp -s -- "$path" "$dbus_src"; then
    WHY+="; it is identical to dist/dbus-1/services/$DBUS_NAME (a manual install): remove it and re-run"
  fi
  if [[ $path == "$module_manifest_dst" ]]; then
    WHY+="; it looks like a manual install of the Maxine module manifest: remove it (and the library its library line names, if that was installed by hand too, e.g. ~/.local/lib/omarchy-broadcast/modules/$MODULE_LIB) and re-run"
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
  # The microphone files are always candidates too, so a lost manifest or a
  # changed XDG_STATE_HOME does not leave them behind; check_targets still
  # refuses one without the marker, and a missing one is skipped.
  # So are the module's two files, for the same reason.
  targets=("${bin_dsts[@]}" "$unit_dst" "$dbus_dst" "$module_lib_dst" "$module_manifest_dst" "$mic_unit_dst")
  for c in "${MIC_CONFS[@]}"; do targets+=("$mic_conf_dir/$c"); done
  # Also what an earlier run wrote under other XDG directories.
  if [[ -f $manifest ]]; then
    while IFS= read -r line; do
      path=$(manifest_path "$line")
      [[ -n $path ]] && targets+=("$path")
    done <"$manifest"
  fi
  # (The shim files of an earlier version are only ever removed through its
  # manifest: a copy there that this script did not install is none of its
  # business.)
  mapfile -t targets < <(printf '%s\n' "${targets[@]}" | sort -u)

  # Removing the file of an enabled unit leaves a dangling enable link:
  # disable it first.
  # A microphone instance's link (pipewire.service.wants/omarchy-broadcast-mic@
  # rnnoise.service) points at the template unit file.
  enabled=0 mic_links=()
  for link in "$config_dir"/systemd/user/*.wants/{"$UNIT_NAME",omarchy-broadcast-mic@*.service}; do
    [[ -L $link ]] || continue
    unit=$UNIT_NAME
    [[ ${link##*/} == "$UNIT_NAME" ]] || unit=$MIC_UNIT_NAME
    for path in "${targets[@]}"; do
      if [[ ${path##*/} == "$unit" && $(readlink -- "$link") == "$path" ]]; then
        printf 'install-user: %s is still enabled (%s)\n' "${link##*/}" "$link" >&2
        if [[ $unit == "$UNIT_NAME" ]]; then enabled=1; else mic_links+=("${link##*/}"); fi
      fi
    done
  done
  if ((enabled || ${#mic_links[@]})); then
    printf 'install-user: run this first, then re-run --uninstall:\n' >&2
    ((!enabled)) || printf '  systemctl --user disable --now omarchy-broadcastd\n' >&2
    if ((${#mic_links[@]})); then
      mapfile -t mic_links < <(printf '%s\n' "${mic_links[@]}" | sort -u)
      printf '  systemctl --user disable --now' >&2
      printf ' %q' "${mic_links[@]}" >&2
      printf '\n' >&2
    fi
    exit 1
  fi

  check_targets "${targets[@]}" "$manifest"

  removed=0 mic_removed=0
  lib_dirs=() leaf_dirs=()
  for path in "${targets[@]}"; do
    [[ -e $path ]] || continue
    run rm -f -- "$path"
    removed=1
    if is_legacy_shim "$path"; then lib_dirs+=("${path%/*}"); fi
    case ${path##*/} in
      "$MIC_UNIT_NAME") mic_removed=1 ;;
      omarchy-broadcast-mic-*.conf) mic_removed=1 leaf_dirs+=("${path%/*}") ;;
      "$MODULE_LIB" | "$MODULE_MANIFEST") leaf_dirs+=("${path%/*}") ;;
    esac
  done
  # The legacy shim's library directory goes once it is empty (rmdir only
  # removes an empty one).
  if ((${#lib_dirs[@]})); then
    while IFS= read -r path; do
      if [[ ${path##*/} == omarchy-broadcast ]]; then
        run rmdir --ignore-fail-on-non-empty -- "$path"
      fi
    done < <(printf '%s\n' "${lib_dirs[@]}" | sort -u)
  fi
  # So do the omarchy-broadcast/{pipewire,modules,modules.d} directories of
  # the microphone and the module, and then omarchy-broadcast itself, once
  # the files are gone. Each is removed only while it exists and only if it
  # is empty.
  # The module's two directories are always tried, so an empty one left by an
  # interrupted first install goes too.
  leaf_dirs+=("$module_dir" "${module_manifest_dst%/*}")
  parent_dirs=()
  while IFS= read -r path; do
    case $path in
      */omarchy-broadcast/pipewire | */omarchy-broadcast/modules | */omarchy-broadcast/modules.d)
        if [[ -d $path ]]; then run rmdir --ignore-fail-on-non-empty -- "$path"; fi
        parent_dirs+=("${path%/*}")
        ;;
    esac
  done < <(printf '%s\n' "${leaf_dirs[@]}" | sort -u)
  while IFS= read -r path; do
    if [[ -d $path ]]; then run rmdir --ignore-fail-on-non-empty -- "$path"; fi
  done < <(printf '%s\n' "${parent_dirs[@]}" | sort -u)
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
  systemctl --user stop omarchy-broadcastd    # if it is still running
  systemctl --user daemon-reload
EOF
  if ((${#lib_dirs[@]})); then
    if ((dry_run)); then
      legacy_shim_note "The os-release shim of an earlier install-user.sh would be removed."
    else
      legacy_shim_note "The os-release shim of an earlier install-user.sh is gone."
    fi
  fi
  if ((mic_removed)); then
    cat <<EOF

The noise-removed microphone is gone; stop any instance still running and
reload:
  systemctl --user stop 'omarchy-broadcast-mic@*.service'
  systemctl --user daemon-reload
EOF
  fi
  exit 0
fi

# --- install ---------------------------------------------------------------

[[ -f $unit_src ]] || die "missing $unit_src"
[[ -f $dbus_src ]] || die "missing $dbus_src"
[[ -f $module_manifest_src ]] || die "missing $module_manifest_src"

# The shipped unit must have exactly one ExecStart naming the daemon, whatever
# its path (/usr/bin for the package, %h/.cargo/bin in older checkouts).
exec_re='^ExecStart=.*omarchy-broadcastd$'
n=$(grep -c -- "$exec_re" "$unit_src" || true)
((n == 1)) || {
  printf 'install-user: expected exactly one line matching %s in %s, found %s\n' "$exec_re" "$unit_src" "$n" >&2
  exit 2
}

# The module manifest names the library by absolute path, in a TOML basic
# string: the shipped one must have exactly one such line, which is rewritten
# to the installed library.
lib_re='^library = .*$'
n=$(grep -c -- "$lib_re" "$module_manifest_src" || true)
((n == 1)) || {
  printf 'install-user: expected exactly one line matching %s in %s, found %s\n' "$lib_re" "$module_manifest_src" "$n" >&2
  exit 2
}
# That path goes into a TOML string, and (with --mic) into ExecStart: the data
# dir must need no escaping in either.
[[ $data_dir =~ ^/[A-Za-z0-9._/@+-]*$ ]] ||
  die "data dir '$data_dir' contains characters that would need escaping in the module manifest and in ExecStart; set XDG_DATA_HOME to a plain path"

# The microphone unit's ExecStart names its configuration by file name, which
# PipeWire does not look up under XDG_DATA_HOME: it is rewritten to the
# installed path (the data dir is checked above).
mic_exec_re='^ExecStart=/usr/bin/pipewire -c omarchy-broadcast-mic-%i\.conf$'
if ((want_mic)); then
  [[ -f $mic_unit_src ]] || die "missing $mic_unit_src"
  for c in "${MIC_CONFS[@]}"; do
    [[ -f $repo/dist/pipewire/$c ]] || die "missing $repo/dist/pipewire/$c"
  done
  n=$(grep -c -- "$mic_exec_re" "$mic_unit_src" || true)
  ((n == 1)) || {
    printf 'install-user: expected exactly one line matching %s in %s, found %s\n' "$mic_exec_re" "$mic_unit_src" "$n" >&2
    exit 2
  }
fi

# Previously installed paths that still exist stay in the manifest, so
# --uninstall finds them even if the XDG directories changed since. The
# current targets are left out here and listed afresh below. The microphone
# files stay listed when this run does not install them (no --mic). The shim
# files of an earlier version are removed below when they are still the
# files it installed (owned), and left out of the manifest; one that changed
# since stays in place and listed, so --uninstall still reports it.
kept=() legacy_rm=()
if [[ -f $manifest ]]; then
  while IFS= read -r line; do
    path=$(manifest_path "$line")
    [[ -n $path && -e $path ]] || continue
    case $path in
      "$unit_dst" | "$dbus_dst" | "$module_lib_dst" | "$module_manifest_dst") continue ;;
    esac
    for dst in "${bin_dsts[@]}"; do [[ $path == "$dst" ]] && continue 2; done
    for dst in "${mic_dsts[@]}"; do [[ $path == "$dst" ]] && continue 2; done
    if is_legacy_shim "$path"; then
      if owned "$path"; then
        # Listed once per checksum while an install was in progress: once here.
        for dst in "${legacy_rm[@]}"; do [[ $path == "$dst" ]] && continue 2; done
        legacy_rm+=("$path")
        continue
      fi
      warn "leaving $path in place: $WHY"
    fi
    kept+=("$line")
  done <"$manifest"
fi

check_targets "${bin_dsts[@]}" "$unit_dst" "$dbus_dst" "$module_lib_dst" "$module_manifest_dst" "${mic_dsts[@]}" "$manifest"

if ((build)); then
  run cargo build --manifest-path "$repo/Cargo.toml" --release --locked -p omarchy-broadcastd -p omarchy-broadcast || {
    printf 'install-user: cargo build failed; nothing was changed\n' >&2
    exit 1
  }
  # Separate from the daemon's build: one invocation unifies the features of
  # its packages, which could change the daemon.
  run cargo build --manifest-path "$repo/Cargo.toml" --release --locked -p "$MODULE_CRATE" || {
    printf 'install-user: cargo build failed; nothing was changed\n' >&2
    exit 1
  }
fi
if ((!dry_run)); then
  for b in "${BINARIES[@]}"; do
    [[ -x $target_dir/release/$b ]] || die "no built binary at $target_dir/release/$b (run without --no-build)"
  done
  [[ -f $target_dir/release/$MODULE_LIB ]] || die "no built module at $target_dir/release/$MODULE_LIB (run without --no-build)"
fi

# The shim files of an earlier version go before the manifest is rewritten
# without them, so an interrupted run leaves each one either removed or
# still listed. Their directory goes once it is empty.
if ((${#legacy_rm[@]})); then
  run rm -f -- "${legacy_rm[@]}"
  while IFS= read -r path; do
    if [[ ${path##*/} == omarchy-broadcast ]]; then
      run rmdir --ignore-fail-on-non-empty -- "$path"
    fi
  done < <(printf '%s\n' "${legacy_rm[@]%/*}" | sort -u)
fi

# The manifest text: its marker, the kept lines and the given lines.
manifest_text() {
  echo "$MARKER"
  echo "# Paths written by install-user.sh, one per line; a binary as \"<sha256>  <path>\"."
  printf '%s\n' "${kept[@]}" "$@" "$unit_dst" "$dbus_dst" "$module_manifest_dst" "${mic_dsts[@]}" | sort -u
}

# What to install with a checksum in the manifest: source, destination, mode.
srcs=() dsts=() modes=()
for b in "${BINARIES[@]}"; do
  srcs+=("$target_dir/release/$b") dsts+=("$bin_dir/$b") modes+=(755)
done
srcs+=("$target_dir/release/$MODULE_LIB") dsts+=("$module_lib_dst") modes+=(644)

run mkdir -p -- "$bin_dir" "$module_dir"
# Install beside the target, then rename: replacing a running binary in place
# would fail with "Text file busy".
old_sums=() new_sums=()
for i in "${!dsts[@]}"; do
  tmp=${dsts[i]%/*}/.${dsts[i]##*/}.install-user
  ((dry_run)) || tmp_files+=("$tmp")
  run install -m"${modes[i]}" -T -- "${srcs[i]}" "$tmp"
  if ((dry_run)); then
    new_sums+=("${dsts[i]}")
  else
    new_sums+=("$(file_sum "$tmp")  ${dsts[i]}")
    # check_targets verified anything already there against the manifest.
    [[ ! -e ${dsts[i]} ]] || old_sums+=("$(file_sum "${dsts[i]}")  ${dsts[i]}")
  fi
done

# The manifest lists both checksums while the binaries are replaced, so an
# interrupted run leaves either version recognised and a re-run may proceed.
put_text "$manifest" manifest_text "${old_sums[@]}" "${new_sums[@]}"
for i in "${!dsts[@]}"; do
  run mv -f -T -- "${dsts[i]%/*}/.${dsts[i]##*/}.install-user" "${dsts[i]}"
done
put_text "$manifest" manifest_text "${new_sums[@]}"

unit_text() {
  echo "$MARKER"
  echo "# Generated from dist/systemd/$UNIT_NAME; ExecStart points at the installed binary."
  sed "s|$exec_re|ExecStart=$bin_dir/omarchy-broadcastd|" "$unit_src"
}
put_text "$unit_dst" unit_text

dbus_text() {
  echo "$MARKER"
  cat "$dbus_src"
}
put_text "$dbus_dst" dbus_text

module_manifest_text() {
  echo "$MARKER"
  echo "# Generated from crates/$MODULE_CRATE/$MODULE_MANIFEST; library points at the installed module."
  # The source's leading comment block describes the packaged install: left out.
  awk 'started || !/^#/ { started = 1; print }' "$module_manifest_src" |
    sed "s|$lib_re|library = \"$module_lib_dst\"|"
}
put_text "$module_manifest_dst" module_manifest_text

if ((want_mic)); then
  mic_conf_text() {
    echo "$MARKER"
    echo "# Generated from dist/pipewire/$1"
    cat "$repo/dist/pipewire/$1"
  }
  for c in "${MIC_CONFS[@]}"; do put_text "$mic_conf_dir/$c" mic_conf_text "$c"; done
  mic_unit_text() {
    echo "$MARKER"
    echo "# Generated from dist/systemd/$MIC_UNIT_NAME; ExecStart points at the installed configuration."
    sed "s|$mic_exec_re|ExecStart=/usr/bin/pipewire -c $mic_conf_dir/omarchy-broadcast-mic-%i.conf|" "$mic_unit_src"
  }
  put_text "$mic_unit_dst" mic_unit_text
fi

if [[ -e $config_dir/systemd/user/$UNIT_NAME ]]; then
  warn "$config_dir/systemd/user/$UNIT_NAME exists and takes precedence over $unit_dst, which it shadows; to use the new unit: systemctl --user disable omarchy-broadcastd, then remove that file"
fi
if ((want_mic)) && [[ -e $config_dir/systemd/user/$MIC_UNIT_NAME ]]; then
  warn "$config_dir/systemd/user/$MIC_UNIT_NAME exists and takes precedence over $mic_unit_dst, which it shadows; to use the new unit: systemctl --user disable omarchy-broadcast-mic@rnnoise.service omarchy-broadcast-mic@deepfilter.service, then remove that file"
fi
case ":${PATH:-}:" in
  *":$bin_dir:"*) ;;
  *) warn "$bin_dir is not on PATH; the omarchy-broadcast CLI must be called by its full path" ;;
esac

wp_src=$repo/dist/systemd/wireplumber.service.d/omarchy-broadcast.conf
done_msg="Installed."
((!dry_run)) || done_msg="Dry run: nothing was changed."
cat <<EOF

$done_msg
Next steps (not run by this script):
  systemctl --user daemon-reload
  systemctl --user enable --now omarchy-broadcastd
  (already running: systemctl --user restart omarchy-broadcastd)

Optional, start WirePlumber after the daemon so PipeWire lists the virtual
camera (see $(printf '%q' "$wp_src")):
  install -Dm644 $(printf '%q' "$wp_src") $(printf '%q' "$config_dir/systemd/user/wireplumber.service.d/omarchy-broadcast.conf")
  systemctl --user daemon-reload

The Omarchy bar plugin is a separate project: see the omarchy-broadcast-plugin
repository and its README.
EOF
module_state="is installed"
((!dry_run)) || module_state="would be installed"
cat <<EOF

The NVIDIA Maxine video effects module $module_state ($(printf '%q' "$module_lib_dst")).
Without the Maxine SDK the daemon lists it greyed out, with the reason. To use
it (docs/setup/maxine.md, section 3), install the maxine-vfx-sdk-local package
(built from the omarchy-nvidia-sdks repository), then run the daemon with it:
  install -Dm644 $(printf '%q' "$maxine_src") $(printf '%q' "$maxine_dropin")
  systemctl --user daemon-reload
  systemctl --user restart omarchy-broadcastd
A running daemon never reloads a module library: restart it to pick up a
rebuilt module.
EOF
if ((${#legacy_rm[@]})); then
  if ((dry_run)); then
    legacy_shim_note "The os-release shim of an earlier install-user.sh would be removed: the
maxine-vfx-sdk-local package provides it now."
  else
    legacy_shim_note "The os-release shim of an earlier install-user.sh was removed: the
maxine-vfx-sdk-local package provides it now."
  fi
fi
if ((want_mic)); then
  cat <<EOF

Optional, the noise-removed microphone (see docs/setup/microphone.md); run
one instance at a time:
  systemctl --user daemon-reload
  systemctl --user enable --now omarchy-broadcast-mic@rnnoise.service
    (RNNoise, the default; needs the package noise-suppression-for-voice)
  or, with DeepFilterNet (needs the package deepfilternet-ladspa):
  systemctl --user enable --now omarchy-broadcast-mic@deepfilter.service
EOF
fi
