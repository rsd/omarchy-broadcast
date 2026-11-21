#!/usr/bin/env bash
# aur.sh — make the AUR copy of packaging/arch/PKGBUILD (pkgbase
# omarchy-broadcast-git) and its .SRCINFO, with the install scriptlet
# packaging/arch/omarchy-broadcast.install, copied unchanged; or, with
# --bin, the AUR recipe of the prebuilt packages of a release (pkgbase
# omarchy-broadcast-bin).
#
# The PKGBUILD in this repository builds from the local checkout that holds
# it (source=git+file://...), for development. The AUR copy is the same file
# with three blocks replaced, each matched exactly: the header paragraph about
# the local checkout, the build hint, and the source= line with its comment,
# which becomes git+https://github.com/rsd/omarchy-broadcast.git. The script
# fails when a block it expects is not in the PKGBUILD exactly once, so a
# change to one of those lines must be made here too (the source= block, which
# scripts/release-bin.sh replaces as well, in scripts/pkgbuild-blocks.sh).
#
# Usage:
#   scripts/aur.sh               # prints the AUR PKGBUILD
#   scripts/aur.sh --srcinfo     # prints its .SRCINFO
#   scripts/aur.sh --check DIR   # fails, with a diff, when DIR's PKGBUILD,
#                                # .SRCINFO, omarchy-broadcast.install or
#                                # LICENSE is not the generated one, or when
#                                # DIR holds any other file
#   scripts/aur.sh --aur DIR     # writes PKGBUILD, .SRCINFO,
#                                # omarchy-broadcast.install and LICENSE (0BSD)
#                                # into DIR, the top of a git clone of the AUR
#                                # repository
#                                # (ssh://aur@aur.archlinux.org/omarchy-broadcast-git.git);
#                                # refuses when DIR holds any other file
#   scripts/aur.sh --bin RELEASE_DIR [--srcinfo | --check DIR | --aur DIR]
#                                # the same for omarchy-broadcast-bin
#                                # (ssh://aur@aur.archlinux.org/omarchy-broadcast-bin.git),
#                                # from the release directory that
#                                # scripts/release-bin.sh made
#
# The .SRCINFO is made by `LC_ALL=C makepkg --printsrcinfo` on the AUR copy,
# with the install scriptlet beside it (makepkg checks that the install=
# file exists) in an empty directory under mktemp, in an environment holding only
# PATH and an empty HOME (no user makepkg.conf is read). --aur commits
# nothing: DIR's git index is left to the maintainer.
#
# The -bin recipe. RELEASE_DIR holds the release assets
# <name>-<version>-x86_64.tar.zst, their SHA256SUMS and, in pkg/, the -git
# packages they were made from. Its packages are those of the assets that
# SHA256SUMS lists (the core, omarchy-broadcast, is required; a Maxine module
# whose asset the release left out is left out here too, and so is any
# optdepend on it), in the order of the -git split package: <name>-bin for
# each asset <name>. Nothing about a
# package is typed here: pkgdesc (" (prebuilt)" appended), license, depends,
# optdepends, provides and conflicts are read from the .PKGINFO of the
# <name>-git package in pkg/. Each <name>-bin package keeps the -git one's
# depends (none may name a -git package) and provides (which include <name>,
# and for the core omarchy-broadcast-module-abi=<major>.<minor>), and
# conflicts with its conflicts (<name>) and with <name>-git; an optdepend
# on a <name>-git package names <name> instead, which either kind provides.
# It has no build() or check() and no makedepends: package() unpacks the
# asset's usr/ tree into $pkgdir as it is, so arch=(x86_64) and
# options=(!strip !debug !lto); noextract keeps makepkg from unpacking the
# assets into one $srcdir. The sources are the assets of the GitHub release
# v<version>, pinned by the sums of SHA256SUMS; the core's install= is
# omarchy-broadcast.install, which must equal the core -git package's
# .INSTALL. Before writing anything, every asset must match SHA256SUMS, and
# hold its -git package's usr/ tree as it is (every entry's type, mode,
# owner, time, size, link target and sha256, compared as bsdtar mtree
# lists), the licence directory usr/share/licenses/<name>-git named
# <name>-bin being the only difference.
#
# Exit status: 0 done (or up to date), 1 stale or not a git clone, 2 usage,
# an expected block missing, a release directory that does not fit (--bin),
# or a makepkg error.
set -euo pipefail

prog=${0##*/}
top=$(cd "$(dirname "$0")/.." && pwd)
pkgbuild=$top/packaging/arch/PKGBUILD
install_file=omarchy-broadcast.install
pkgbase=omarchy-broadcast-git
url=https://github.com/rsd/omarchy-broadcast.git
# shellcheck source=scripts/pkgbuild-blocks.sh
. "$top/scripts/pkgbuild-blocks.sh"

usage() {
    echo "usage: $prog [--bin RELEASE_DIR] [--srcinfo | --check DIR | --aur DIR]" >&2
    exit 2
}

# cmd: the command that regenerates DIR, for the messages.
mode=print dir_arg='' rel_arg='' cmd=$prog
if [[ ${1:-} == --bin ]]; then
    (($# >= 2)) || usage
    rel_arg=$2 pkgbase=omarchy-broadcast-bin cmd="$prog --bin $2"
    shift 2
fi
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

aur_source="source=(\"\$_pkgname::git+$url\")"

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

# The packages of the -bin recipe, by the name of their release asset, in
# the order of the -git split package. An asset of another name is refused.
bin_names=(
    omarchy-broadcast
    omarchy-broadcast-maxine-vfx
    omarchy-broadcast-maxine-ar
    omarchy-broadcast-maxine-afx
    omarchy-broadcast-onnx
)
bin_url=https://github.com/rsd/omarchy-broadcast

# field <.PKGINFO> <key>: every value of <key>, one per line.
field() { sed -n "s/^$2 = //p" "$1"; }
# q <value>: the value single-quoted for bash.
q() { printf "'%s'" "${1//\'/\'\\\'\'}"; }
# array <indent> <name> <newline-separated values>: a bash array, one value
# per line; nothing when there are none.
array() {
    local v list
    mapfile -t list < <(grep . <<<"$3" || true)
    ((${#list[@]} > 0)) || return 0
    printf '%s%s=(\n' "$1" "$2"
    for v in "${list[@]}"; do printf '%s  %s\n' "$1" "$(q "$v")"; done
    printf '%s)\n' "$1"
}
# bin_fail <message>: prints it; the caller returns 2.
bin_fail() { echo "$prog: $1" >&2; }

# bin_pkgbuild <file>: writes the -bin PKGBUILD of the release directory
# $rel_arg to <file>.
bin_pkgbuild() {
    local tool rel sums line sum file name v ver='' pkg info f absent='' mt
    local -a pkgs names=() descs=() licenses=() deps=() optdeps=() provs=() confs=()
    local -A hash=()
    for tool in bsdtar sha256sum; do
        command -v "$tool" >/dev/null 2>&1 || { bin_fail "$tool not found"; return 2; }
    done
    if ! rel=$(cd -- "$rel_arg" 2>/dev/null && pwd -P); then
        bin_fail "$rel_arg is not a directory"
        return 2
    fi
    sums=$rel/SHA256SUMS
    [[ -f $sums ]] || { bin_fail "$sums does not exist (scripts/release-bin.sh writes it)"; return 2; }
    if ! (cd "$rel" && sha256sum --quiet --strict -c SHA256SUMS) >&2; then
        bin_fail "the assets in $rel do not match its SHA256SUMS"
        return 2
    fi
    while IFS= read -r line; do
        if [[ ! $line =~ ^([0-9a-f]{64})\ [\ *](.+)$ ]]; then
            bin_fail "$sums: not a sha256sum line: $line"
            return 2
        fi
        sum=${BASH_REMATCH[1]} file=${BASH_REMATCH[2]}
        if [[ ! $file =~ ^(omarchy-broadcast(-maxine-vfx|-maxine-ar|-maxine-afx|-onnx)?)-([0-9][0-9A-Za-z.+_]*)-x86_64\.tar\.zst$ ]]; then
            bin_fail "$sums: $file is not a release asset <name>-<version>-x86_64.tar.zst of omarchy-broadcast"
            return 2
        fi
        name=${BASH_REMATCH[1]} v=${BASH_REMATCH[3]}
        if [[ -n $ver && $v != "$ver" ]]; then
            bin_fail "$sums: assets of two versions, $ver and $v"
            return 2
        fi
        ver=$v
        [[ -z ${hash[$name]:-} ]] || { bin_fail "$sums: two $name assets"; return 2; }
        hash[$name]=$sum
    done <"$sums"
    if [[ -z ${hash[omarchy-broadcast]:-} ]]; then
        bin_fail "$sums lists no core asset (omarchy-broadcast-<version>-x86_64.tar.zst)"
        return 2
    fi

    # absent: the packages the release left out, as an ERE alternation; an
    # optdepend on one of them is dropped.
    for name in "${bin_names[@]}"; do
        [[ -n ${hash[$name]:-} ]] || absent+=${absent:+|}$name
    done
    for name in "${bin_names[@]}"; do
        if [[ -z ${hash[$name]:-} ]]; then
            echo "$prog: the release has no $name asset: $name-bin is left out, and so is any optdepend on it" >&2
            continue
        fi
        file=$name-$ver-x86_64.tar.zst
        pkgs=()
        for pkg in "$rel/pkg/$name-git-$ver.r"*-x86_64.pkg.tar.zst; do
            [[ ! -e $pkg ]] || pkgs+=("$pkg")
        done
        if ((${#pkgs[@]} != 1)); then
            bin_fail "$rel/pkg holds ${#pkgs[@]} packages $name-git-$ver.r*-x86_64.pkg.tar.zst, not one (the -git package of $file)"
            return 2
        fi
        pkg=${pkgs[0]} info=$tmp/PKGINFO
        if ! bsdtar -xOf "$pkg" .PKGINFO >"$info" 2>/dev/null || [[ ! -s $info ]]; then
            bin_fail "$pkg: no .PKGINFO"
            return 2
        fi
        if [[ $(field "$info" pkgname) != "$name-git" || $(field "$info" pkgbase) != omarchy-broadcast-git ||
            $(field "$info" arch) != x86_64 || $(field "$info" pkgver) != "$ver".r*-* ||
            -n $(field "$info" backup) ]]; then
            bin_fail "$pkg: its .PKGINFO is not that of $name-git $ver.r* (pkgbase omarchy-broadcast-git, x86_64, no backup files)"
            return 2
        fi
        # The asset is the package's usr/ tree, with the licence directory
        # of the -bin package.
        bsdtar -tf "$pkg" | grep -v '^\.' |
            sed "s|^usr/share/licenses/$name-git/|usr/share/licenses/$name-bin/|" |
            LC_ALL=C sort >"$tmp/want"
        bsdtar -tf "$rel/$file" | LC_ALL=C sort >"$tmp/got"
        if grep -qv '^usr/' "$tmp/want" || ! grep -qx "usr/share/licenses/$name-bin/" "$tmp/want"; then
            bin_fail "$pkg: holds more than usr/ and its metadata, or no licence directory"
            return 2
        fi
        if ! diff -u --label "$name-git (licence directory renamed)" --label "$file" "$tmp/want" "$tmp/got" >&2; then
            bin_fail "$rel/$file does not hold the files of $pkg"
            return 2
        fi
        # And each file as it is: type, mode, owner, time, size, link
        # target and content of every entry.
        mt='!all,type,mode,uid,gid,uname,gname,time,size,sha256,link'
        if ! bsdtar -cf "$tmp/want.mtree" --format=mtree --options="$mt" \
            --exclude .PKGINFO --exclude .BUILDINFO --exclude .MTREE --exclude .INSTALL \
            -s "|^usr/share/licenses/$name-git/|usr/share/licenses/$name-bin/|" \
            -s "|^usr/share/licenses/$name-git\$|usr/share/licenses/$name-bin|" "@$pkg" ||
            ! bsdtar -cf "$tmp/got.mtree" --format=mtree --options="$mt" "@$rel/$file"; then
            bin_fail "bsdtar could not list $pkg or $rel/$file"
            return 2
        fi
        LC_ALL=C sort -o "$tmp/want.mtree" "$tmp/want.mtree"
        LC_ALL=C sort -o "$tmp/got.mtree" "$tmp/got.mtree"
        if ! grep -q 'sha256digest=' "$tmp/want.mtree" ||
            ! diff -u --label "$name-git (licence directory renamed)" --label "$file" "$tmp/want.mtree" "$tmp/got.mtree" >&2; then
            bin_fail "$rel/$file does not hold the files of $pkg as they are (modes, owners, times or contents differ)"
            return 2
        fi
        if [[ $name == omarchy-broadcast ]]; then
            if ! bsdtar -xOf "$pkg" .INSTALL 2>/dev/null | cmp -s - "${pkgbuild%/*}/$install_file"; then
                bin_fail "${pkgbuild%/*}/$install_file is not the .INSTALL of $pkg"
                return 2
            fi
        elif bsdtar -tf "$pkg" .INSTALL >/dev/null 2>&1; then
            bin_fail "$pkg has an install scriptlet, which $name-bin would not run"
            return 2
        fi
        f=$(field "$info" depend)
        if grep -q '^omarchy-broadcast.*-git\([<>=]\|$\)' <<<"$f"; then
            bin_fail "$pkg depends on a -git package, which a -bin package cannot: $(grep '^omarchy-broadcast.*-git' <<<"$f" | head -n 1)"
            return 2
        fi
        deps+=("$f")
        optdeps+=("$(field "$info" optdepend |
            sed -E 's/^(omarchy-broadcast(-[a-z0-9-]*[a-z0-9])?)-git:/\1:/' |
            { if [[ -n $absent ]]; then grep -Ev "^($absent)(-git)?(:|\$)" || true; else cat; fi; })")
        f=$(field "$info" provides)
        grep -qx "$name" <<<"$f" || { bin_fail "$pkg does not provide $name"; return 2; }
        provs+=("$f")
        f=$(field "$info" conflict)
        grep -qx "$name" <<<"$f" || { bin_fail "$pkg does not conflict with $name"; return 2; }
        confs+=("$f"$'\n'"$name-git")
        descs+=("$(field "$info" pkgdesc) (prebuilt)")
        licenses+=("$(field "$info" license)")
        names+=("$name")
    done

    local n=${#names[@]} i
    {
        echo '# Maintainer: Raul Dias <raul@dias.com.br>'
        echo '#'
        echo "# Prebuilt Arch packages of omarchy-broadcast ($bin_url),"
        echo "# from the assets of its release v$ver. Generated by scripts/aur.sh --bin"
        echo '# in that repository, from the -git packages the release was made of: do'
        echo '# not edit it by hand; report problems there.'
        echo '#'
        echo '# Each <name>-bin package installs the usr/ tree of the release asset'
        echo '# <name>-<pkgver>-x86_64.tar.zst as it is: the files of <name>-git, with'
        echo '# the licence directory named <name>-bin. Each provides <name> and'
        echo '# conflicts with <name> and <name>-git, so that either kind meets the'
        echo '# other packages. The NVIDIA Maxine SDKs and ONNX Runtime are not part of'
        echo '# these packages: the module packages depend on the packages that hold'
        echo '# them, as the -git ones do.'
        echo
        echo 'pkgbase=omarchy-broadcast-bin'
        array '' pkgname "$(printf '%s-bin\n' "${names[@]}")"
        echo "pkgver=$ver"
        echo 'pkgrel=1'
        echo "pkgdesc=$(q "${descs[0]}")"
        echo "url='$bin_url'"
        echo "arch=('x86_64')"
        array '' license "${licenses[0]}"
        echo "options=('!strip' '!debug' '!lto')"
        echo 'source=('
        for name in "${names[@]}"; do
            # shellcheck disable=SC2016 # the PKGBUILD's text, expanded by makepkg
            printf '  "%s-$pkgver-x86_64.tar.zst::%s/releases/download/v$pkgver/%s-$pkgver-x86_64.tar.zst"\n' \
                "$name" "$bin_url" "$name"
        done
        echo ')'
        echo 'sha256sums=('
        for name in "${names[@]}"; do printf "  '%s'\n" "${hash[$name]}"; done
        echo ')'
        # shellcheck disable=SC2016 # the PKGBUILD's text, expanded by makepkg
        echo '# Extracted, every asset would land in the same $srcdir and their usr/'
        echo '# trees would merge: each package() unpacks its own.'
        echo 'noextract=('
        # shellcheck disable=SC2016 # the PKGBUILD's text, expanded by makepkg
        for name in "${names[@]}"; do printf '  "%s-$pkgver-x86_64.tar.zst"\n' "$name"; done
        echo ')'
        for ((i = 0; i < n; i++)); do
            echo
            echo "package_${names[i]}-bin() {"
            echo "  pkgdesc=$(q "${descs[i]}")"
            array '  ' license "${licenses[i]}"
            array '  ' depends "${deps[i]}"
            array '  ' optdepends "${optdeps[i]}"
            array '  ' provides "${provs[i]}"
            array '  ' conflicts "${confs[i]}"
            ((i != 0)) || echo "  install=$install_file"
            echo
            # shellcheck disable=SC2016 # the PKGBUILD's text, expanded by makepkg
            printf '  bsdtar -xpf "$srcdir/%s-$pkgver-x86_64.tar.zst" -C "$pkgdir" usr\n' "${names[i]}"
            echo '}'
        done
    } >"$1"
    bash -n "$1" || { bin_fail "the generated PKGBUILD does not parse (a bug of $prog)"; return 2; }
}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# generate: writes $tmp/out/PKGBUILD, $tmp/out/.SRCINFO, the install
# scriptlet and $tmp/out/LICENSE.
generate() {
    command -v makepkg >/dev/null 2>&1 || { echo "$prog: makepkg not found" >&2; return 2; }
    mkdir -p "$tmp/out" "$tmp/home"
    if [[ -n $rel_arg ]]; then
        bin_pkgbuild "$tmp/out/PKGBUILD" || return
    else
        aur_pkgbuild "$tmp/out/PKGBUILD" || return
        # shellcheck disable=SC2016 # the PKGBUILD's text, expanded by makepkg
        if ! grep -qxF '  install=$_pkgname.install' "$tmp/out/PKGBUILD"; then
            echo "$prog: the PKGBUILD does not set install=\$_pkgname.install ($install_file)" >&2
            return 2
        fi
    fi
    cp -- "${pkgbuild%/*}/$install_file" "$tmp/out/$install_file" || return 2
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

files=(PKGBUILD .SRCINFO "$install_file" LICENSE)

# extra <dir>: fails when <dir> holds anything but .git and the files: an AUR
# repository holds exactly what is pushed.
extra() {
    local f found=0
    while IFS= read -r -d '' f; do
        f=${f##*/}
        case $f in .git | PKGBUILD | .SRCINFO | "$install_file" | LICENSE) continue ;; esac
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
            echo "$prog: $1/$f does not exist: run $cmd --aur $1" >&2
            status=1
        elif ! diff -u --label "$1/$f" --label "generated $f" "$1/$f" "$tmp/out/$f"; then
            echo "$prog: $1/$f is stale: run $cmd --aur $1" >&2
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
