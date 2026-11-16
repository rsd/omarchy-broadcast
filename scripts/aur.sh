#!/usr/bin/env bash
# aur.sh — make the AUR copy of packaging/arch/PKGBUILD (pkgbase
# omarchy-broadcast-git) and its .SRCINFO.
#
# The PKGBUILD in this repository builds from the local checkout that holds
# it (source=git+file://...), for development. The AUR copy is the same file
# with three blocks replaced, each matched exactly: the header paragraph about
# the local checkout, the build hint, and the source= line with its comment,
# which becomes git+https://github.com/rsd/omarchy-broadcast.git. The script
# fails when a block it expects is not in the PKGBUILD exactly once, so a
# change to one of those lines must be made here too.
#
# Usage:
#   scripts/aur.sh               # prints the AUR PKGBUILD
#   scripts/aur.sh --srcinfo     # prints its .SRCINFO
#   scripts/aur.sh --check DIR   # fails, with a diff, when DIR's PKGBUILD,
#                                # .SRCINFO or LICENSE is not the generated one,
#                                # or when DIR holds any other file
#   scripts/aur.sh --aur DIR     # writes PKGBUILD, .SRCINFO and LICENSE (0BSD)
#                                # into DIR, the top of a git clone of the AUR
#                                # repository
#                                # (ssh://aur@aur.archlinux.org/omarchy-broadcast-git.git);
#                                # refuses when DIR holds any other file
#
# The .SRCINFO is made by `LC_ALL=C makepkg --printsrcinfo` on the AUR copy,
# alone in an empty directory under mktemp, in an environment holding only
# PATH and an empty HOME (no user makepkg.conf is read). --aur commits
# nothing: DIR's git index is left to the maintainer.
#
# Exit status: 0 done (or up to date), 1 stale or not a git clone, 2 usage,
# an expected block missing, or a makepkg error.
set -euo pipefail

prog=${0##*/}
top=$(cd "$(dirname "$0")/.." && pwd)
pkgbuild=$top/packaging/arch/PKGBUILD
pkgbase=omarchy-broadcast-git
url=https://github.com/rsd/omarchy-broadcast.git

usage() {
    echo "usage: $prog [--srcinfo | --check DIR | --aur DIR]" >&2
    exit 2
}

mode=print dir_arg=''
case $# in
    0) ;;
    1) [[ $1 == --srcinfo ]] || usage; mode=srcinfo ;;
    2)
        case $1 in
            --check | --aur) mode=${1#--} dir_arg=$2 ;;
            *) usage ;;
        esac
        ;;
    *) usage ;;
esac

# The blocks of the local PKGBUILD and their AUR replacements, whole lines.
local_header='# Arch packages of omarchy-broadcast, built from the local git checkout that
# holds this file (two directories up). Only committed content is built:
# makepkg clones the repository, so uncommitted changes are not included.
# makepkg rewrites pkgver= below on each build; that change need not be
# committed. The committed pkgver= is the workspace version in Cargo.toml: a
# version bump changes both files, and scripts/check-all.sh fails while they
# differ. scripts/aur.sh makes the AUR copy of this file, which builds from
# the public repository instead (docs/setup/daemon.md, "Publishing to the AUR").'
aur_header='# Arch packages of omarchy-broadcast (https://github.com/rsd/omarchy-broadcast),
# built from the latest commit of its default branch; makepkg sets pkgver
# from that commit. This file is generated from packaging/arch/PKGBUILD in
# that repository: report problems there.'

local_build='# Build (from packaging/arch):   makepkg -s'
aur_build='# Build (in this directory):     makepkg -s'

# shellcheck disable=SC2016 # the PKGBUILD's text, expanded by makepkg
local_source='# The checkout that holds this PKGBUILD; resolved at build time, so the
# package builds from wherever the repository is cloned.
source=("$_pkgname::git+file://$(realpath -- "$startdir/../..")")'
aur_source="source=(\"\$_pkgname::git+$url\")"

# replace <text> <old> <new>: prints <text> with the block <old> (whole
# lines) replaced by <new>; fails unless <old> occurs exactly once.
replace() {
    local text=$'\n'$1$'\n' old=$'\n'$2$'\n' new=$'\n'$3$'\n' rest count=0
    # Counted one by one, keeping each match's last newline: two adjacent
    # copies share it.
    rest=$text
    while [[ $rest == *"$old"* ]]; do
        count=$((count + 1))
        rest=$'\n'${rest#*"$old"}
    done
    if ((count != 1)); then
        printf '%s: %s has this block %d times, not once:\n%s\n' \
            "$prog" "$pkgbuild" "$count" "$2" >&2
        return 2
    fi
    text=${text/"$old"/"$new"}
    text=${text#$'\n'}
    printf '%s' "${text%$'\n'}"
}

# aur_pkgbuild <file>: writes the AUR PKGBUILD to <file>.
aur_pkgbuild() {
    local text
    text=$(<"$pkgbuild")
    text=$(replace "$text" "$local_header" "$aur_header") || return
    text=$(replace "$text" "$local_build" "$aur_build") || return
    text=$(replace "$text" "$local_source" "$aur_source") || return
    if grep -q 'git+file:\|startdir' <<<"$text"; then
        echo "$prog: the AUR PKGBUILD still refers to the local checkout" >&2
        return 2
    fi
    printf '%s\n' "$text" >"$1"
}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# generate: writes $tmp/out/PKGBUILD and $tmp/out/.SRCINFO.
generate() {
    command -v makepkg >/dev/null 2>&1 || { echo "$prog: makepkg not found" >&2; return 2; }
    mkdir -p "$tmp/out" "$tmp/home"
    aur_pkgbuild "$tmp/out/PKGBUILD" || return
    if ! (cd "$tmp/out" && env -i PATH="$PATH" HOME="$tmp/home" LC_ALL=C makepkg --printsrcinfo >.SRCINFO); then
        echo "$prog: makepkg --printsrcinfo failed" >&2
        return 2
    fi
    cat >"$tmp/out/LICENSE" <<'EOF'
Copyright (c) 2026 Raul Dias <raul@dias.com.br>

Permission to use, copy, modify, and/or distribute this software for any
purpose with or without fee is hereby granted.

THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES WITH
REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF MERCHANTABILITY
AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR ANY SPECIAL, DIRECT,
INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES WHATSOEVER RESULTING FROM
LOSS OF USE, DATA OR PROFITS, WHETHER IN AN ACTION OF CONTRACT, NEGLIGENCE OR
OTHER TORTIOUS ACTION, ARISING OUT OF OR IN CONNECTION WITH THE USE OR
PERFORMANCE OF THIS SOFTWARE.
EOF
}

files=(PKGBUILD .SRCINFO LICENSE)

# extra <dir>: fails when <dir> holds anything but .git and the files: an AUR
# repository holds exactly what is pushed.
extra() {
    local f found=0
    while IFS= read -r -d '' f; do
        f=${f##*/}
        case $f in .git | PKGBUILD | .SRCINFO | LICENSE) continue ;; esac
        echo "$prog: $1/$f is not part of the AUR package" >&2
        found=1
    done < <(find "$1" -mindepth 1 -maxdepth 1 -print0)
    if ((found)); then
        echo "$prog: remove the files above from $1 first" >&2
        return 1
    fi
}

# check <dir>: whether <dir> holds the generated files; prints a diff when not.
check() {
    local f status=0
    for f in "${files[@]}"; do
        if [[ ! -f $1/$f ]]; then
            echo "$prog: $1/$f does not exist: run $prog --aur $1" >&2
            status=1
        elif ! diff -u --label "$1/$f" --label "generated $f" "$1/$f" "$tmp/out/$f"; then
            echo "$prog: $1/$f is stale: run $prog --aur $1" >&2
            status=1
        fi
    done
    extra "$1" || status=1
    return "$status"
}

case $mode in
    print)
        generate
        cat "$tmp/out/PKGBUILD"
        ;;
    srcinfo)
        generate
        cat "$tmp/out/.SRCINFO"
        ;;
    check)
        generate
        check "$dir_arg" || exit 1
        echo "$dir_arg is up to date"
        ;;
    aur)
        # The top of a git work tree, not a directory inside one (such as
        # this repository's): an AUR repository has the files at its top.
        if ! dir=$(cd -- "$dir_arg" 2>/dev/null && pwd -P) ||
            ! git_top=$(git -C "$dir" rev-parse --show-toplevel 2>/dev/null) ||
            [[ $(cd -- "$git_top" && pwd -P) != "$dir" ]]; then
            echo "$prog: $dir_arg is not the top of a git clone (git clone ssh://aur@aur.archlinux.org/$pkgbase.git)" >&2
            exit 1
        fi
        generate
        extra "$dir" || exit 1
        for f in "${files[@]}"; do
            cp -- "$tmp/out/$f" "$dir/.$f.tmp"
            mv -f -- "$dir/.$f.tmp" "$dir/$f"
        done
        check "$dir"
        echo "wrote ${files[*]} into $dir; review, commit and push there"
        ;;
esac
