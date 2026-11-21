# shellcheck shell=bash
# pkgbuild-blocks.sh — sourced by scripts/aur.sh and scripts/release-bin.sh,
# not run: the source= block of packaging/arch/PKGBUILD, which both replace,
# and the exact-block replacement they replace it with. A change to that
# block in the PKGBUILD is made here, once, for both.
#
# The caller sets prog (its name, for messages) and pkgbuild (the PKGBUILD
# named in messages).

# The source= line of the local PKGBUILD with its comment, whole lines.
# shellcheck disable=SC2016,SC2034 # the PKGBUILD's text, expanded by makepkg; used by the callers
local_source='# The checkout that holds this PKGBUILD; resolved at build time, so the
# package builds from wherever the repository is cloned.
source=("$_pkgname::git+file://$(realpath -- "$startdir/../..")")'

# replace <text> <old> <new>: prints <text> with the block <old> (whole
# lines) replaced by <new>; fails unless <old> occurs exactly once.
# shellcheck disable=SC2154 # prog and pkgbuild are the caller's
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
