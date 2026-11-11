#!/usr/bin/env bash
# Runs a command against a private, headless PipeWire and WirePlumber, for
# the microphone's integration tests. It never touches the user's session:
#
#  * everything runs inside `dbus-run-session` with a private session bus
#    that listens inside the sandbox and activates nothing;
#  * XDG_RUNTIME_DIR, PIPEWIRE_RUNTIME_DIR, XDG_CONFIG_HOME, XDG_STATE_HOME,
#    XDG_DATA_HOME and XDG_CACHE_HOME all point into a fresh temporary
#    directory, removed at exit, so neither the user's PipeWire socket nor
#    ~/.config, nor WirePlumber's saved state, are ever read or written;
#    OMARCHY_BROADCAST_AUDIO_SOCKET is unset, so the audio worker's socket
#    is the sandbox's too;
#  * the PipeWire server loads no device support (no ALSA, bluez, v4l2 or
#    libcamera; no udev), and WirePlumber runs the `policy` profile (no
#    hardware monitors), with an isolated configuration in the temporary
#    directory.
#
# The server carries two virtual microphones of equal priority,
# obc-test-mic-a and obc-test-mic-b (Audio/Source/Virtual, mono, 48 kHz), and
# one of them, obc-test-mic-a, is the configured default source. A
# `pw-cat --playback` of a test tone feeds each of them.
#
# usage: scripts/mic-pw-sandbox.sh [--plugin PATH] [--conf PATH] -- COMMAND...
#
#   --plugin  the forwarding plugin (libomarchy_broadcast_mic.so) the mic host
#             loads; default: target/debug/libomarchy_broadcast_mic.so
#   --conf    the mic host configuration to adapt; default:
#             dist/pipewire/omarchy-broadcast-mic.conf
#
# COMMAND runs with, besides the variables above:
#   OBC_PW_SANDBOX         the temporary directory (= XDG_RUNTIME_DIR)
#   OBC_PW_SANDBOX_MIC_CONF the mic host configuration with the plugin's
#                           absolute path; start it with `pipewire -c "$it"`
#                           (the command starts and stops the mic host itself)
# The exit status is COMMAND's. The logs are copied to $OBC_PW_SANDBOX_LOGS
# when that names a directory.
set -euo pipefail

# Nothing inherited may steer the sandbox: the audio worker's socket would
# otherwise point outside it, and the inner marker would skip the private
# bus. The inner run is told by its own argument instead.
unset OMARCHY_BROADCAST_AUDIO_SOCKET OBC_PW_SANDBOX_INNER

REPO=$(cd "$(dirname "$0")/.." && pwd)
PLUGIN=$REPO/target/debug/libomarchy_broadcast_mic.so
CONF=$REPO/dist/pipewire/omarchy-broadcast-mic.conf
INNER=
while [ $# -gt 0 ]; do
    case $1 in
        --plugin) PLUGIN=$2; shift 2 ;;
        --conf) CONF=$2; shift 2 ;;
        --inner) INNER=$2; shift 2 ;;
        --) shift; break ;;
        *) echo "usage: $0 [--plugin PATH] [--conf PATH] -- COMMAND..." >&2; exit 2 ;;
    esac
done
[ $# -gt 0 ] || { echo "no command given" >&2; exit 2; }

for tool in dbus-run-session pipewire wireplumber pw-cat pw-cli pw-dump pw-metadata; do
    command -v "$tool" >/dev/null || { echo "$tool is not installed" >&2; exit 2; }
done
[ -f "$PLUGIN" ] || { echo "no plugin at $PLUGIN (build obc-mic-shim)" >&2; exit 2; }
PLUGIN=$(realpath "$PLUGIN")
[ -f "$CONF" ] || { echo "no mic host configuration at $CONF" >&2; exit 2; }

# The sandbox directory must be a fresh temporary one, never the user's.
check_sandbox() {
    case $1 in
        "${OBC_PW_SANDBOX_TMP:-/tmp}"/obc-pw.*) ;;
        *) echo "refusing: $1 is not a sandbox temporary directory" >&2; exit 2 ;;
    esac
    [ -d "$1" ] || { echo "refusing: $1 is not a directory" >&2; exit 2; }
    if [ -n "${XDG_RUNTIME_DIR:-}" ] && [ "$1" = "$XDG_RUNTIME_DIR" ]; then
        echo "refusing: the sandbox would be the user's runtime directory" >&2
        exit 2
    fi
}

if [ -z "$INNER" ]; then
    # Short on purpose: $RT/pipewire-0 must fit in sun_path (108 bytes).
    RT=$(mktemp -d "${OBC_PW_SANDBOX_TMP:-/tmp}/obc-pw.XXXXXX")
    check_sandbox "$RT"
    trap 'rm -rf "$RT"' EXIT
    # The private session bus listens inside the sandbox, and activates
    # nothing (no service directories).
    cat >"$RT/bus.conf" <<BUS
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-BUS Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:dir=$RT</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
BUS
    set +e
    env OBC_PW_SANDBOX_OUTER_BUS="${DBUS_SESSION_BUS_ADDRESS:-}" \
        dbus-run-session --config-file="$RT/bus.conf" -- \
        "$0" --inner "$RT" --plugin "$PLUGIN" --conf "$CONF" -- "$@"
    rc=$?
    set -e
    exit $rc
fi

# Inside dbus-run-session: the sandbox the outer run made, and a bus of its
# own in it, never the caller's.
RT=$INNER
check_sandbox "$RT"
case ${DBUS_SESSION_BUS_ADDRESS:-} in
    unix:path="$RT"/*) ;;
    *) echo "refusing: the session bus ${DBUS_SESSION_BUS_ADDRESS:-(none)} is not the sandbox's" >&2; exit 2 ;;
esac
if [ "$DBUS_SESSION_BUS_ADDRESS" = "${OBC_PW_SANDBOX_OUTER_BUS:-}" ]; then
    echo "refusing: the session bus is the caller's" >&2
    exit 2
fi
unset OBC_PW_SANDBOX_OUTER_BUS

PIDS=()
# shellcheck disable=SC2329 # invoked by the EXIT trap
cleanup() {
    local rc=$?
    for pid in "${PIDS[@]}"; do kill "$pid" 2>/dev/null || true; done
    for pid in "${PIDS[@]}"; do wait "$pid" 2>/dev/null || true; done
    if [ -n "${OBC_PW_SANDBOX_LOGS:-}" ] && [ -d "${OBC_PW_SANDBOX_LOGS}" ]; then
        cp "$RT"/*.log "$OBC_PW_SANDBOX_LOGS"/ 2>/dev/null || true
    fi
    # The outer run removes $RT once the bus is gone.
    exit "$rc"
}
trap cleanup EXIT

unset PIPEWIRE_REMOTE PULSE_SERVER PIPEWIRE_CONFIG_DIR WIREPLUMBER_CONFIG_DIR
export XDG_RUNTIME_DIR=$RT PIPEWIRE_RUNTIME_DIR=$RT
export XDG_CONFIG_HOME=$RT/config XDG_STATE_HOME=$RT/state XDG_DATA_HOME=$RT/data
export XDG_CACHE_HOME=$RT/cache
mkdir -p "$XDG_CONFIG_HOME/wireplumber/wireplumber.conf.d" "$XDG_STATE_HOME" "$XDG_DATA_HOME" \
    "$XDG_CACHE_HOME"
export OBC_PW_SANDBOX=$RT
# GIO in WirePlumber would otherwise start gvfs on the private bus.
export GIO_USE_VFS=local GSETTINGS_BACKEND=memory

mic() {
    # $1 node.name, $2 description
    cat <<EOF
    { factory = adapter
        args = {
            factory.name     = support.null-audio-sink
            node.name        = "$1"
            node.description = "$2"
            media.class      = Audio/Source/Virtual
            audio.rate       = 48000
            audio.channels   = 1
            audio.position   = [ MONO ]
            priority.session = 2109
            object.linger    = true
        }
    }
EOF
}

cat >"$RT/server.conf" <<EOF
# The sandbox's server: no device support, a dummy driver, two virtual mics.
context.properties = {
    core.daemon = true
    core.name   = pipewire-0
    default.clock.rate          = 48000
    default.clock.allowed-rates = [ 48000 ]
    default.clock.quantum       = 1024
    default.clock.min-quantum   = 32
    default.clock.max-quantum   = 2048
    support.dbus    = false
    mem.allow-mlock = false
    log.level       = 2
}
context.spa-libs = {
    audio.convert.* = audioconvert/libspa-audioconvert
    support.*       = support/libspa-support
}
context.modules = [
    { name = libpipewire-module-protocol-native }
    { name = libpipewire-module-metadata }
    { name = libpipewire-module-spa-node-factory }
    { name = libpipewire-module-client-node }
    { name = libpipewire-module-client-device }
    { name = libpipewire-module-access }
    { name = libpipewire-module-adapter }
    { name = libpipewire-module-link-factory }
    { name = libpipewire-module-session-manager }
]
context.objects = [
    { factory = spa-node-factory
        args = {
            factory.name    = support.node.driver
            node.name       = Dummy-Driver
            node.group      = pipewire.dummy
            priority.driver = 20000
        }
    }
$(mic obc-test-mic-a "Test mic A")
$(mic obc-test-mic-b "Test mic B")
]
EOF

# WirePlumber: only what the policy needs; nothing of the user's.
cat >"$XDG_CONFIG_HOME/wireplumber/wireplumber.conf.d/90-sandbox.conf" <<'EOF'
wireplumber.profiles = {
  policy = {
    monitor.alsa       = disabled
    monitor.alsa-midi  = disabled
    monitor.bluez      = disabled
    monitor.bluez-midi = disabled
    monitor.v4l2       = disabled
    monitor.libcamera  = disabled
    support.reserve-device = disabled
    support.portal-permissionstore = disabled
    support.logind     = disabled
  }
}
EOF

# The mic host: the shipped configuration with the plugin's absolute path.
sed -e "s#plugin = \"libomarchy_broadcast_mic\"#plugin = \"$PLUGIN\"#" "$CONF" >"$RT/mic.conf"
grep -q "plugin = \"$PLUGIN\"" "$RT/mic.conf" || { echo "cannot set the plugin path in $CONF" >&2; exit 2; }
export OBC_PW_SANDBOX_MIC_CONF=$RT/mic.conf

PIPEWIRE_CONFIG_DIR=$RT pipewire -c "$RT/server.conf" >"$RT/pipewire.log" 2>&1 &
PIDS+=($!)
for _ in $(seq 100); do [ -S "$RT/pipewire-0" ] && break; sleep 0.05; done
[ -S "$RT/pipewire-0" ] || { echo "pipewire did not start"; cat "$RT/pipewire.log"; exit 1; }

WIREPLUMBER_DEBUG=${WIREPLUMBER_DEBUG:-2} wireplumber -p policy >"$RT/wireplumber.log" 2>&1 &
PIDS+=($!)
for _ in $(seq 100); do
    pw-metadata -n default 0 2>/dev/null | grep -q . && pw-cli ls Node 2>/dev/null | grep -q obc-test-mic-b && break
    sleep 0.05
done
pw-metadata -n default 0 >/dev/null 2>&1 || { echo "wireplumber did not start"; cat "$RT/wireplumber.log"; exit 1; }

pw-metadata -n default 0 default.configured.audio.source '{ "name": "obc-test-mic-a" }' Spa:String:JSON >/dev/null

# A tone into each mic (pw-cat links to nothing; pw-link feeds the mic).
feed() {
    # $1 mic, $2 frequency
    python3 - "$2" >"$RT/$1.wav" <<'PY'
import math, struct, sys, wave
f = float(sys.argv[1]); rate = 48000; n = rate * 2
w = wave.open(sys.stdout.buffer, "wb"); w.setnchannels(1); w.setsampwidth(2); w.setframerate(rate)
w.writeframes(b"".join(struct.pack("<h", int(8000 * math.sin(2 * math.pi * f * i / rate))) for i in range(n)))
PY
    # Loop the clip so the tone never ends.
    (while :; do pw-cat --playback --target 0 -P "{ node.name = obc-feed-$1 node.dont-reconnect = true }" "$RT/$1.wav" || sleep 0.2; done) >"$RT/feed-$1.log" 2>&1 &
    PIDS+=($!)
}
feed obc-test-mic-a 440
feed obc-test-mic-b 660
# Keep the feeds linked to their mics (each loop restarts pw-cat).
(while :; do
    for m in obc-test-mic-a obc-test-mic-b; do
        pw-link "obc-feed-$m:output_MONO" "$m:input_MONO" >/dev/null 2>&1 || true
    done
    sleep 0.2
done) &
PIDS+=($!)

set +e
"$@"
rc=$?
set -e
exit $rc
