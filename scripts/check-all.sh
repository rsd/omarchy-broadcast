#!/usr/bin/env bash
# check-all.sh — every check the repository's changes must pass, in one run.
#
# Runs, in order, and stops at the first failure:
#   1. the PKGBUILD pkgver= values match the workspace version in Cargo.toml
#      (a makepkg-appended ".r<N>.g<HASH>" suffix is accepted);
#   2. cargo fmt, clippy (-D warnings) and test of the workspace;
#   3. the same three for the example modules, which are built outside the
#      workspace, as third-party modules would be (examples/module-hello and
#      examples/module-hello-audio);
#   4. clippy and test of obc-pipeline and omarchy-broadcastd with the
#      PipeWire feature (omarchy-broadcastd/pipewire);
#   5. shellcheck over the committed shell scripts (git ls-files '*.sh').
#
# It needs no GPU and no camera: the tests that need hardware or the Maxine
# SDK are ignored by default, and MAXINE_VFX_ROOT is left as it is.
#
# Usage:
#   scripts/check-all.sh
#
# Needs: a Rust toolchain with rustfmt and clippy, git, GStreamer (core,
# gst-plugins-base, gst-plugins-good), dbus (the tests run
# /usr/bin/dbus-daemon), libpipewire and clang (bindgen) for the PipeWire
# feature, pipewire (its client.conf), and shellcheck on PATH or uvx
# (uvx --from shellcheck-py shellcheck).
#
# Exit status: 0 when every step passed; otherwise the failing step's status.

set -euo pipefail

cd "$(dirname "$0")/.."

export CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-always}"

step() {
    printf '\n==> %s\n' "$1"
}

# The committed pkgver= is the workspace version; makepkg's pkgver() appends
# .rN.gHASH to it, so that form is accepted too.
check_pkgbuild_versions() {
    local cargo_version pkgbuild=packaging/arch/PKGBUILD pkgbuild_version
    cargo_version=$(sed -n '/^\[workspace\.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml)
    if [ -z "$cargo_version" ]; then
        echo "no version in Cargo.toml [workspace.package]" >&2
        return 1
    fi
    pkgbuild_version=$(sed -n 's/^pkgver=//p' "$pkgbuild")
    case $pkgbuild_version in
        "$cargo_version" | "$cargo_version".r*) ;;
        *)
            echo "$pkgbuild pkgver=$pkgbuild_version differs from Cargo.toml version $cargo_version" >&2
            return 1
            ;;
    esac
}

# One example module, outside the workspace, through its own manifest.
check_example() {
    local manifest=$1/Cargo.toml
    cargo fmt --manifest-path "$manifest" -- --check
    cargo clippy --manifest-path "$manifest" --all-targets --locked -- -D warnings
    cargo test --manifest-path "$manifest" --locked
}

run_shellcheck() {
    local -a shellcheck_cmd
    # --version, not just command -v: a version manager's shim can be on PATH
    # without a version installed behind it.
    if shellcheck --version >/dev/null 2>&1; then
        shellcheck_cmd=(shellcheck)
    elif command -v uvx >/dev/null 2>&1; then
        shellcheck_cmd=(uvx --from shellcheck-py shellcheck)
    else
        echo "neither shellcheck nor uvx is on PATH; install one of them" >&2
        return 1
    fi
    git ls-files -z '*.sh' | xargs -0 "${shellcheck_cmd[@]}"
}

step "PKGBUILD version"
check_pkgbuild_versions

step "Format"
cargo fmt --all -- --check

step "Clippy"
cargo clippy --workspace --all-targets --locked -- -D warnings

step "Test"
cargo test --workspace --locked

step "Example module (examples/module-hello)"
check_example examples/module-hello

step "Example audio module (examples/module-hello-audio)"
check_example examples/module-hello-audio

step "Clippy, PipeWire feature"
cargo clippy -p obc-pipeline -p omarchy-broadcastd --all-targets \
    --features omarchy-broadcastd/pipewire --locked -- -D warnings

step "Test, PipeWire feature"
cargo test -p obc-pipeline -p omarchy-broadcastd \
    --features omarchy-broadcastd/pipewire --locked

step "Shellcheck"
run_shellcheck

printf '\nAll checks passed.\n'
