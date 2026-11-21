#!/usr/bin/env bash
# release-bin.sh — build the assets of a GitHub release of omarchy-broadcast,
# from which the prebuilt AUR packages (pkgbase omarchy-broadcast-bin,
# scripts/aur.sh --bin) install.
#
# Usage:
#   scripts/release-bin.sh [--nocheck] [--out DIR]
#
# The release is of the commit HEAD names when the script starts, and of the
# workspace version in that commit's Cargo.toml; the script refuses when the
# pkgver= of that commit's packaging/arch/PKGBUILD is another. It warns, and
# goes on, when the work tree has uncommitted changes: they are not built.
# That PKGBUILD and its install scriptlet are copied, with git show, into a
# scratch directory (mktemp), with the source= line replaced (exactly, as
# scripts/aur.sh does; scripts/pkgbuild-blocks.sh) by a git+file:// source
# of this repository pinned to the commit, and built there with `makepkg -f`
# (--nocheck is passed on: it skips check(), the workspace test suite).
# BUILDDIR, PKGDEST, SRCDEST and LOGDEST are in the scratch directory, which
# is removed at the end, and PKGEXT is .pkg.tar.zst whatever makepkg.conf
# says. The script installs nothing and runs no -s: the build dependencies
# must be installed already. The packages are found by name and version,
# <name>-git-<version>.r<count>.g<hash>-<pkgrel>: the hash is the one
# pkgver() abbreviated in makepkg's clone, which must be a prefix of the
# commit's.
#
# Of the five -git packages of the split package (the -debug one is
# skipped), each <name>-git gives one asset, <name>-<version>-x86_64.tar.zst:
# the package's usr/ tree as it is (owners, modes and times of the package),
# with its licence directory usr/share/licenses/<name>-git named
# <name>-bin, the licence directory of the -bin package that installs it.
# Written into DIR (default target/release-bin/v<version> at the top of this
# repository), which must not exist or be empty: the assets, SHA256SUMS
# (sha256sum format, over the assets) and, in DIR/pkg, the -git packages.
# They are made in a temporary directory beside DIR, which becomes DIR only
# when the release is complete; on any failure it is removed and DIR is
# left as it was.
#
# The legal gate, run on every asset after it is made: the NVIDIA Maxine
# SDKs and ONNX Runtime are licensed separately and are never part of a
# release. Every ELF file of an asset is one of a list per package (the
# core's three programs and two libraries, each module's library; any other
# ELF file fails the gate), and each of them must be an ELF file with a
# dynamic section whose NEEDED libraries (readelf -d) are all on its own
# allowlist (the modules and the LADSPA plugin: libc, libm, libgcc_s and
# the dynamic loader only; each program: those and the system libraries it
# links today) and whose defined dynamic symbols (nm -D --defined-only) are
# exactly its own list (a module: obc_module_abi_version and
# obc_module_entry; the plugin: ladspa_descriptor; a program: none). The
# libraries are also scanned (strings) for markers that only code of the
# SDKs linked in statically holds (see sdk_markers). A tool that fails is a
# failed gate. Every file must be one the package is expected to hold (a
# whitelist per package; no other .so, no *.trtpkg, *.engine, *.plan or
# *.onnx file, no models/ directory, nothing but plain files and
# directories). The gate prints what it found. A Maxine module that fails
# it is left out of the release, and said so; when the core or the ONNX
# module fails it, the script fails.
#
# At the end it prints, and does not run, the commands that publish the
# release: the tag, the GitHub release and the AUR update.
#
# Exit status: 0 done, 1 refused (version mismatch, DIR not empty, an asset
# that fails the gate, a package that does not fit), 2 usage, a missing
# tool or a makepkg error.
set -euo pipefail
shopt -s nullglob

prog=${0##*/}
top=$(cd "$(dirname "$0")/.." && pwd)
repo=rsd/omarchy-broadcast

usage() {
    echo "usage: $prog [--nocheck] [--out DIR]" >&2
    exit 2
}
die() { # die <status> <message>
    echo "$prog: $2" >&2
    exit "$1"
}

nocheck=0 out=''
while (($#)); do
    case $1 in
        --nocheck) nocheck=1; shift ;;
        --out) (($# >= 2)) || usage; out=$2; shift 2 ;;
        --out=*) out=${1#*=}; shift ;;
        *) usage ;;
    esac
done

for tool in git makepkg bsdtar readelf nm strings od sha256sum zstd; do
    command -v "$tool" >/dev/null 2>&1 || die 2 "$tool not found"
done

# The commit, once: what is read from it and what is built are the same.
sha=$(git -C "$top" rev-parse --verify 'HEAD^{commit}')
short=$(git -C "$top" rev-parse --short "$sha")
pkgbuild="packaging/arch/PKGBUILD of $short"
ver=$(git -C "$top" show "$sha:Cargo.toml" |
    sed -n '/^\[workspace\.package\]/,/^\[/s/^version = "\(.*\)"$/\1/p')
[[ $ver =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die 1 "Cargo.toml of $short: no workspace version (got '$ver')"
recipe=$(git -C "$top" show "$sha:packaging/arch/PKGBUILD")
pkgver=$(sed -n 's/^pkgver=//p' <<<"$recipe")
[[ $pkgver == "$ver" ]] ||
    die 1 "$pkgbuild has pkgver=$pkgver, but its Cargo.toml says $ver: bump both"
if [[ -n $(git -C "$top" status --porcelain --untracked-files=no) ]]; then
    echo "$prog: warning: the work tree has uncommitted changes; they are not built (the release is of $short)" >&2
fi
count=$(git -C "$top" rev-list --count "$sha")

out=${out:-$top/target/release-bin/v$ver}
out=${out%/}
[[ -n $out && ${out##*/} != . && ${out##*/} != .. ]] || die 2 "--out: not a directory name: '$out'"
if [[ -e $out ]] && [[ ! -d $out || -n $(find "$out" -mindepth 1 -maxdepth 1 -print -quit) ]]; then
    die 1 "$out exists and is not an empty directory: remove it first"
fi
[[ $out == */* ]] || out=./$out
mkdir -p -- "${out%/*}"
out=$(cd -- "${out%/*}" && pwd)/${out##*/}

# shellcheck source=scripts/pkgbuild-blocks.sh
. "$top/scripts/pkgbuild-blocks.sh"

# stage: the release being made, beside $out; it becomes $out at the end.
stage=''
cleanup() {
    rm -rf -- "$scratch"
    [[ -z $stage ]] || rm -rf -- "$stage"
}
scratch=$(mktemp -d -t omarchy-broadcast-release.XXXXXX)
trap cleanup EXIT
mkdir -p "$scratch"/{recipe,build,pkgdest,srcdest,log,tree}

# The recipe: the committed PKGBUILD, building the commit from this
# repository, with its install scriptlet beside it.
text=$(replace "$recipe" "$local_source" "source=(\"\$_pkgname::git+file://$top#commit=$sha\")") ||
    die 2 "the source= block of $pkgbuild is not the expected one (scripts/pkgbuild-blocks.sh)"
printf '%s\n' "$text" >"$scratch/recipe/PKGBUILD"
git -C "$top" show "$sha:packaging/arch/omarchy-broadcast.install" >"$scratch/recipe/omarchy-broadcast.install"

margs=(-f)
((nocheck == 0)) || margs+=(--nocheck)
echo "$prog: building $short (v$ver) with makepkg ${margs[*]} in $scratch"
if ! (cd "$scratch/recipe" &&
    BUILDDIR=$scratch/build PKGDEST=$scratch/pkgdest SRCDEST=$scratch/srcdest LOGDEST=$scratch/log \
        PKGEXT=.pkg.tar.zst makepkg "${margs[@]}"); then
    die 2 "makepkg failed"
fi

# The split packages, by asset name, in the PKGBUILD's order; each asset's
# ELF files, and whether a failed gate leaves the asset out (Maxine) or
# fails the release.
names=(
    omarchy-broadcast
    omarchy-broadcast-maxine-vfx
    omarchy-broadcast-maxine-ar
    omarchy-broadcast-maxine-afx
    omarchy-broadcast-onnx
)
declare -A elves=(
    [omarchy-broadcast]='usr/bin/omarchy-broadcastd usr/bin/omarchy-broadcast usr/bin/omarchy-broadcast-audiod usr/lib/omarchy-broadcast/modules/libobc_module_voice_cpu.so usr/lib/ladspa/libomarchy_broadcast_mic.so'
    [omarchy-broadcast-maxine-vfx]='usr/lib/omarchy-broadcast/modules/libobc_module_maxine.so'
    [omarchy-broadcast-maxine-ar]='usr/lib/omarchy-broadcast/modules/libobc_module_maxine_ar.so'
    [omarchy-broadcast-maxine-afx]='usr/lib/omarchy-broadcast/modules/libobc_module_afx.so'
    [omarchy-broadcast-onnx]='usr/lib/omarchy-broadcast/modules/libobc_module_onnx.so'
)
declare -A manifests=(
    [omarchy-broadcast]=voice-cpu.toml
    [omarchy-broadcast-maxine-vfx]=maxine-vfx.toml
    [omarchy-broadcast-maxine-ar]=maxine-ar.toml
    [omarchy-broadcast-maxine-afx]=maxine-afx.toml
    [omarchy-broadcast-onnx]=onnx.toml
)

# The packages makepkg made, but -debug ones:
# <name>-git-<version>.r<count>.g<hash>-<pkgrel> of the five names, all of
# one pkgver-pkgrel, <count> the commit's and <hash> a prefix of it (abbreviated
# in makepkg's clone, which may abbreviate it otherwise than this repository).
full=''
vre=${ver//./\\.}
for f in "$scratch/pkgdest"/*; do
    b=${f##*/}
    [[ $b != *-debug-* ]] || continue
    re="^(omarchy-broadcast(-maxine-vfx|-maxine-ar|-maxine-afx|-onnx)?)-git-($vre\\.r([0-9]+)\\.g([0-9a-f]+)-[0-9]+)-x86_64\\.pkg\\.tar\\.zst\$"
    [[ $b =~ $re ]] || die 1 "makepkg made $b, which is not one of the five packages $ver.r$count.g$short"
    v=${BASH_REMATCH[3]}
    if [[ ${BASH_REMATCH[4]} != "$count" || ${#BASH_REMATCH[5]} -lt 4 || $sha != "${BASH_REMATCH[5]}"* ]]; then
        die 1 "makepkg made $b, which is not of $short (r$count)"
    fi
    [[ -z $full || $v == "$full" ]] || die 1 "makepkg made packages of two versions, $full and $v"
    full=$v
done
declare -A built=()
for name in "${names[@]}"; do
    found=("$scratch/pkgdest/$name-git-$ver.r"*-x86_64.pkg.tar.zst)
    ((${#found[@]} == 1)) ||
        die 1 "makepkg made ${#found[@]} packages $name-git-$ver.r*-x86_64.pkg.tar.zst, not one"
    built[$name]=${found[0]}
done

# The legal gate's allowlists, per ELF file: the libraries it may need
# (NEEDED) and the dynamic symbols it defines, all of them. The modules and
# the LADSPA plugin need only the C runtime; each program also the system
# libraries it links today. Any other library fails the gate: a new one is
# added here, after a look at its licence.
base_needed='libc.so.6 libm.so.6 libgcc_s.so.1 ld-linux-x86-64.so.2'
module_symbols='obc_module_abi_version obc_module_entry'
declare -A needed_ok=(
    [usr/bin/omarchy-broadcastd]="$base_needed libpipewire-0.3.so.0 libgstapp-1.0.so.0 libgstbase-1.0.so.0 libgstvideo-1.0.so.0 libgstreamer-1.0.so.0 libgobject-2.0.so.0 libglib-2.0.so.0 libgio-2.0.so.0"
    [usr/bin/omarchy-broadcast]="$base_needed"
    [usr/bin/omarchy-broadcast-audiod]="$base_needed libpipewire-0.3.so.0 libasound.so.2"
    [usr/lib/ladspa/libomarchy_broadcast_mic.so]="$base_needed"
    [usr/lib/omarchy-broadcast/modules/libobc_module_voice_cpu.so]="$base_needed"
    [usr/lib/omarchy-broadcast/modules/libobc_module_maxine.so]="$base_needed"
    [usr/lib/omarchy-broadcast/modules/libobc_module_maxine_ar.so]="$base_needed"
    [usr/lib/omarchy-broadcast/modules/libobc_module_afx.so]="$base_needed"
    [usr/lib/omarchy-broadcast/modules/libobc_module_onnx.so]="$base_needed"
)
declare -A symbols_ok=(
    [usr/bin/omarchy-broadcastd]=''
    [usr/bin/omarchy-broadcast]=''
    [usr/bin/omarchy-broadcast-audiod]=''
    [usr/lib/ladspa/libomarchy_broadcast_mic.so]=ladspa_descriptor
    [usr/lib/omarchy-broadcast/modules/libobc_module_voice_cpu.so]=$module_symbols
    [usr/lib/omarchy-broadcast/modules/libobc_module_maxine.so]=$module_symbols
    [usr/lib/omarchy-broadcast/modules/libobc_module_maxine_ar.so]=$module_symbols
    [usr/lib/omarchy-broadcast/modules/libobc_module_afx.so]=$module_symbols
    [usr/lib/omarchy-broadcast/modules/libobc_module_onnx.so]=$module_symbols
)
# sdk_markers: strings that only the SDKs' own code holds, so that a
# library (*.so) holding one has SDK code linked in statically, which
# neither NEEDED nor the defined symbols would show. Our modules dlopen the
# SDKs and look their entry points up by name, so they hold, as plain
# strings, the names of those entry points (NvVFX_CreateEffect, NvAR_*,
# NvAFX_*, NvCV_*, NvCVImage_*, OrtGetApiBase, ORT_*), of the SDK libraries
# (libcudart, libnvinfer, libonnxruntime, ...) and words such as CUDA and
# TensorRT: none of those is a marker. The markers are what the SDKs' code
# carries and a caller does not: the source paths of ONNX Runtime's
# assertion messages (onnxruntime/core/), the C++ type names of ONNX
# Runtime and TensorRT in their RTTI (namespaces onnxruntime and nvinfer1,
# mangled), the copyright line of NVIDIA's libraries and the error text of
# the CUDA runtime. None of them is in the modules of v0.57.3.
sdk_markers=(
    'onnxruntime/core/'
    'N11onnxruntime'
    'N8nvinfer1'
    'NVIDIA CORPORATION'
    'NVIDIA Corporation'
    'CUDA driver version is insufficient for CUDA runtime version'
)

# allowed <name> <path>: whether <path>, a file of the asset of <name>, is
# one the package is expected to hold.
allowed() {
    local lic="usr/share/licenses/$1-bin/(LICENSE-MIT|LICENSE-APACHE|THIRD-PARTY-NOTICES)"
    local mod="usr/lib/omarchy-broadcast/modules.d/${manifests[$1]//./\\.}"
    local e re
    for e in ${elves[$1]}; do
        [[ $2 != "$e" ]] || return 0
    done
    re="^($lic|$mod)\$"
    [[ ! $2 =~ $re ]] || return 0
    [[ $1 == omarchy-broadcast ]] || return 1
    re='^(usr/lib/systemd/user/[^/]+\.(service|socket)|usr/lib/udev/rules\.d/[^/]+\.rules|usr/share/dbus-1/services/[^/]+\.service|usr/include/omarchy-broadcast/[^/]+\.h|usr/share/omarchy-broadcast/media/(backgrounds|away)/[^/]+\.(jpg|webm|toml)|usr/share/omarchy-broadcast/pipewire/[^/]+\.conf|usr/share/doc/omarchy-broadcast/[^.][^/]*(/[^./][^/]*)*\.(md|toml|conf))$'
    [[ $2 =~ $re ]]
}

# words <list>: the words of <list>, sorted, one per line.
words() { tr ' ' '\n' <<<"$1" | grep . | LC_ALL=C sort -u || true; }

# elf_gate <tree> <path>: the gate's checks of the ELF file <path> of
# <tree>; prints its findings; fails on any of them, and when a tool fails.
elf_gate() {
    local tree=$1 e=$2 f=$1/$2 magic dyn needed syms extra err=$scratch/gate.err fail=0 m
    if [[ -L $f || ! -f $f ]]; then
        echo "   FAIL $e: missing"
        return 1
    fi
    if [[ -z ${needed_ok[$e]+set} || -z ${symbols_ok[$e]+set} ]]; then
        echo "   FAIL $e: no allowlist for it (a bug of $prog)"
        return 1
    fi
    if ! magic=$(head -c 4 -- "$f" | od -An -tx1 | tr -d ' \n') || [[ $magic != 7f454c46 ]]; then
        echo "   FAIL $e: not an ELF file"
        return 1
    fi
    echo "   $e"
    if ! dyn=$(readelf -dW -- "$f" 2>"$err") || [[ -s $err ]]; then
        echo "     FAIL readelf -d failed: $(tr '\n' ' ' <"$err")"
        return 1
    fi
    if ! grep -q '^Dynamic section at offset' <<<"$dyn"; then
        echo "     FAIL no dynamic section (statically linked?)"
        return 1
    fi
    needed=$(sed -n 's/.*(NEEDED).*\[\(.*\)\]$/\1/p' <<<"$dyn")
    if ! syms=$(nm -D --defined-only -- "$f" 2>"$err") || [[ -s $err ]]; then
        echo "     FAIL nm -D failed: $(tr '\n' ' ' <"$err")"
        return 1
    fi
    syms=$(awk '{print $NF}' <<<"$syms")
    echo "     NEEDED: $(tr '\n' ' ' <<<"$needed")"
    echo "     defined: $(grep -c . <<<"$syms" || true) symbol(s): $(tr '\n' ' ' <<<"$syms")"
    extra=$(LC_ALL=C comm -23 <(words "$needed") <(words "${needed_ok[$e]}"))
    if [[ -n $extra ]]; then
        echo "     FAIL needs a library not on its allowlist: $(tr '\n' ' ' <<<"$extra")"
        fail=1
    fi
    if [[ $(words "$syms") != "$(words "${symbols_ok[$e]}")" ]]; then
        echo "     FAIL defines other symbols than its own (${symbols_ok[$e]:-none})"
        fail=1
    fi
    if [[ $e == *.so ]]; then
        if ! strings -a -n 8 -- "$f" >"$scratch/gate.strings" 2>"$err" || [[ -s $err ]]; then
            echo "     FAIL strings failed: $(tr '\n' ' ' <"$err")"
            return 1
        fi
        for m in "${sdk_markers[@]}"; do
            if grep -qF -- "$m" "$scratch/gate.strings"; then
                echo "     FAIL holds '$m', a marker of SDK code linked in"
                fail=1
            fi
        done
        ((fail)) || echo "     no SDK marker (${#sdk_markers[@]} checked)"
    fi
    return "$fail"
}

# gate <name> <tree>: the legal gate on the unpacked asset of <name>;
# prints its findings; fails when the asset must not be released.
gate() {
    local name=$1 tree=$2 fail=0 path e magic
    echo "== legal gate: $name"
    while IFS= read -r -d '' path; do
        path=${path#"$tree"/}
        if [[ ! -f $tree/$path || -L $tree/$path ]]; then
            echo "   FAIL $path: not a plain file or directory"
            fail=1
            continue
        fi
        case /$path in
            *.trtpkg | *.engine | *.plan | *.onnx | */models/*)
                echo "   FAIL $path: a model or engine file"
                fail=1
                ;;
        esac
        if ! allowed "$name" "$path"; then
            echo "   FAIL $path: not a file this package holds"
            fail=1
        fi
        if ! magic=$(head -c 4 -- "$tree/$path" | od -An -tx1 | tr -d ' \n'); then
            echo "   FAIL $path: cannot be read"
            fail=1
        elif [[ $magic == 7f454c46 && " ${elves[$name]} " != *" $path "* ]]; then
            echo "   FAIL $path: an ELF file this package does not hold"
            fail=1
        fi
    done < <(find "$tree" -mindepth 1 ! -type d -print0 | sort -z)
    while IFS= read -r -d '' path; do
        case /${path#"$tree"} in
            */models | */models/*)
                echo "   FAIL ${path#"$tree"/}: a models directory"
                fail=1
                ;;
        esac
    done < <(find "$tree" -mindepth 1 -type d -print0)
    for e in ${elves[$name]}; do
        elf_gate "$tree" "$e" || fail=1
    done
    if ((fail)); then
        echo "   => $name: FAILED"
        return 1
    fi
    echo "   => $name: passed (only allowed libraries needed, only its own symbols defined, no SDK marker, only the expected files)"
}

stage=$(mktemp -d -- "${out%/*}/.${out##*/}.XXXXXX")
chmod -- "$(umask -S)" "$stage"
mkdir -- "$stage/pkg"
for name in "${names[@]}"; do
    cp -- "${built[$name]}" "$stage/pkg/"
done

assets=() excluded=()
for name in "${names[@]}"; do
    pkg=$stage/pkg/${built[$name]##*/}
    info=$(bsdtar -xOf "$pkg" .PKGINFO)
    if [[ $(sed -n 's/^pkgbase = //p' <<<"$info") != omarchy-broadcast-git ||
        $(sed -n 's/^arch = //p' <<<"$info") != x86_64 ||
        $(sed -n 's/^pkgname = //p' <<<"$info") != "$name-git" ]]; then
        die 1 "$pkg: its .PKGINFO is not that of $name-git (pkgbase omarchy-broadcast-git, x86_64)"
    fi
    while IFS= read -r entry; do
        case $entry in
            .PKGINFO | .BUILDINFO | .MTREE | usr) ;;
            .INSTALL) [[ $name == omarchy-broadcast ]] || die 1 "$pkg: an install scriptlet, which only the core has" ;;
            *) die 1 "$pkg: unexpected top-level entry '$entry'" ;;
        esac
    done < <(bsdtar -tf "$pkg" | sed 's|^\./||; s|/.*||' | sort -u)
    asset=$name-$ver-x86_64.tar.zst
    bsdtar --zstd -cf "$stage/$asset" \
        --exclude .PKGINFO --exclude .BUILDINFO --exclude .MTREE --exclude .INSTALL \
        -s "|^usr/share/licenses/$name-git/|usr/share/licenses/$name-bin/|" \
        -s "|^usr/share/licenses/$name-git\$|usr/share/licenses/$name-bin|" "@$pkg"
    tree=$scratch/tree/$name
    mkdir -p "$tree"
    bsdtar -xpf "$stage/$asset" -C "$tree"
    if gate "$name" "$tree"; then
        assets+=("$asset")
    elif [[ $name == omarchy-broadcast-maxine-* ]]; then
        rm -f -- "$stage/$asset"
        excluded+=("$asset")
        echo "$prog: *** $asset FAILED the legal gate and is LEFT OUT of the release ***" >&2
    else
        rm -f -- "$stage/$asset"
        die 1 "$asset failed the legal gate: no release (see above)"
    fi
done

(cd "$stage" && sha256sum -- "${assets[@]}" | LC_ALL=C sort -k 2) >"$stage/SHA256SUMS"

# The release is complete: it becomes $out (empty or absent, checked above).
[[ ! -d $out ]] || rmdir -- "$out"
mv -T -- "$stage" "$out"
stage=''

echo
echo "$prog: release v$ver of $short in $out:"
(cd "$out" && cat SHA256SUMS)
for asset in "${excluded[@]}"; do
    echo "$prog: *** left out (legal gate): $asset ***"
done
echo
echo "Publish it with (not run):"
printf '  git -C %q tag -a v%s %s -m %q\n' "$top" "$ver" "$sha" "omarchy-broadcast $ver"
printf '  git -C %q push origin v%s\n' "$top" "$ver"
printf '  gh release create v%s --repo %s --verify-tag --title %q --notes %q' \
    "$ver" "$repo" "omarchy-broadcast $ver" \
    "Prebuilt x86_64 files of omarchy-broadcast $ver, installed by the AUR packages omarchy-broadcast-bin (pkgbase). The NVIDIA Maxine SDKs and ONNX Runtime are not included."
for asset in "${assets[@]}"; do printf ' %q' "$out/$asset"; done
printf ' %q\n' "$out/SHA256SUMS"
printf '  %q --bin %q --aur %q\n' "$top/scripts/aur.sh" "$out" "$top/aur/omarchy-broadcast-bin"
printf '  git -C %q add PKGBUILD .SRCINFO omarchy-broadcast.install LICENSE\n' "$top/aur/omarchy-broadcast-bin"
printf '  git -C %q commit -m %q\n' "$top/aur/omarchy-broadcast-bin" "Update to $ver"
printf '  git -C %q push origin HEAD:master\n' "$top/aur/omarchy-broadcast-bin"
