#!/usr/bin/env bash
# dgpu-idle-check.sh — does the NVIDIA dGPU runtime-suspend, and who keeps it awake?
#
# Acceptance check for the on-demand GPU lifecycle ("idle means zero CUDA contexts"); see
# docs/benchmarks/dgpu-idle.md for the method and measured results.
# Reads ONLY sysfs and /proc. It never calls nvidia-smi or NVML: both wake a
# runtime-suspended dGPU and would defeat the measurement.
#
# Usage:
#   scripts/dgpu-idle-check.sh [options] [PCI_BDF]
#
# Options:
#   --device BDF     PCI device to check, e.g. 0000:01:00.0 (same as the bare
#                    positional argument). Default: the first PCI device with
#                    vendor 0x10de and display class 0x03xxxx.
#   --pid PID        Also report whether PID holds /dev/nvidia* fds or maps
#                    libcuda / libnvidia-ml.
#   --wait SECS      Watch power/runtime_status for up to SECS seconds
#                    (foreground, bounded). Exit 0 as soon as it reads
#                    "suspended"; otherwise exit 1 and print what holds the GPU.
#   --interval SECS  Poll interval for --wait (integer, minimum 1). Default 1.
#   --sample SECS    Without --wait: seconds between the two readings used for
#                    the runtime_*_time deltas. Default 2.
#   -h, --help       Show this help.
#
# Exit status:
#   0  report printed; with --wait: runtime_status reached "suspended";
#      with --pid: the pid holds no /dev/nvidia* fd
#   1  --wait: runtime_status did not reach "suspended" in time
#   2  usage error, or no NVIDIA display device found
#   3  --pid: the pid holds /dev/nvidia* fds (checked after --wait succeeds)
#
# Notes:
#   * /proc/<pid>/fd of other users (root daemons such as nvidia-persistenced
#     or nvidia-powerd) is unreadable without root; those are counted and the
#     NVIDIA-named ones listed, but their fds cannot be inspected.
#   * An NVIDIA connector that is connected AND enabled (an external monitor
#     wired to the dGPU) keeps the dGPU awake regardless of any process.
#   * /proc/driver/nvidia/gpus/<BDF>/power is read once, for the RTD3 mode; it
#     is the driver's documented status file for runtime D3 and does not
#     resume the device.
set -euo pipefail

PCI_ROOT=/sys/bus/pci/devices

usage() {
  sed -n '2,/^set -euo pipefail$/{/^set -euo/d;s/^# \{0,1\}//;p}' "$0"
}

die() {
  printf 'dgpu-idle-check: %s\n' "$*" >&2
  exit 2
}

is_uint() { [[ $1 =~ ^[0-9]+$ ]]; }

device=""
target_pid=""
wait_secs=""
interval=1
sample=2

while (($#)); do
  case $1 in
    --device) (($# >= 2)) || die "--device needs a value"; device=$2; shift 2 ;;
    --pid) (($# >= 2)) || die "--pid needs a value"; target_pid=$2; shift 2 ;;
    --wait) (($# >= 2)) || die "--wait needs a value"; wait_secs=$2; shift 2 ;;
    --interval) (($# >= 2)) || die "--interval needs a value"; interval=$2; shift 2 ;;
    --sample) (($# >= 2)) || die "--sample needs a value"; sample=$2; shift 2 ;;
    -h | --help) usage; exit 0 ;;
    -*) die "unknown option: $1 (see --help)" ;;
    *) [[ -z $device ]] || die "more than one device given"; device=$1; shift ;;
  esac
done

[[ -z $target_pid ]] || is_uint "$target_pid" || die "--pid must be a number"
[[ -z $wait_secs ]] || is_uint "$wait_secs" || die "--wait must be whole seconds"
if ! is_uint "$interval" || ((interval < 1)); then die "--interval must be an integer >= 1"; fi
is_uint "$sample" || die "--sample must be whole seconds"

# --- locate the dGPU -------------------------------------------------------

find_nvidia_display() {
  local d vendor class
  for d in "$PCI_ROOT"/*; do
    [[ -r $d/vendor && -r $d/class ]] || continue
    vendor=$(<"$d/vendor")
    class=$(<"$d/class")
    if [[ $vendor == 0x10de && $class == 0x03* ]]; then
      basename "$d"
      return 0
    fi
  done
  return 1
}

if [[ -z $device ]]; then
  device=$(find_nvidia_display) || die "no NVIDIA display-class PCI device (vendor 0x10de, class 0x03xxxx) in $PCI_ROOT"
fi
[[ $device == *:*:*.* ]] || device="0000:$device"
dev_dir=$PCI_ROOT/$device
[[ -d $dev_dir ]] || die "no such PCI device: $dev_dir"
pm_dir=$dev_dir/power
[[ -r $pm_dir/runtime_status ]] || die "$pm_dir/runtime_status is not readable"

read_pm() { cat "$pm_dir/$1" 2>/dev/null || printf 'n/a'; }

# --- who holds /dev/nvidia* ------------------------------------------------

# Fills HOLDER_PIDS (pid list), HOLDER_LINES (formatted report lines),
# UNREADABLE_COUNT and UNREADABLE_NVIDIA (unreadable processes named *nvidia*).
scan_holders() {
  local p pid comm cl readable=() hits line maps flags
  HOLDER_PIDS=()
  HOLDER_LINES=()
  UNREADABLE_COUNT=0
  UNREADABLE_NVIDIA=()
  for p in /proc/[0-9]*; do
    [[ -d $p ]] || continue
    if [[ -r $p/fd && -x $p/fd ]]; then
      readable+=("$p/fd")
    else
      # Kernel threads (empty cmdline) hold no fds; leave them out.
      cl=""
      { IFS= read -r -d '' cl <"$p/cmdline"; } 2>/dev/null || true
      [[ -n $cl ]] || continue
      UNREADABLE_COUNT=$((UNREADABLE_COUNT + 1))
      comm=$(cat "$p/comm" 2>/dev/null || true)
      if [[ $comm == *nvidia* ]]; then UNREADABLE_NVIDIA+=("${p#/proc/}:$comm"); fi
    fi
  done
  ((${#readable[@]})) || return 0
  # One find over every readable fd dir; processes and fds vanish mid-scan,
  # so its errors are expected noise.
  hits=$(find "${readable[@]}" -maxdepth 1 -lname '/dev/nvidia*' -printf '%h %l\n' 2>/dev/null || true)
  [[ -n $hits ]] || return 0
  while read -r pid; do
    [[ -n $pid ]] || continue
    comm=$(cat "/proc/$pid/comm" 2>/dev/null || printf '?')
    line=$(awk -v d="/proc/$pid/fd" '$1 == d { n[$2]++; t++ }
      END { s = ""; for (k in n) s = s sprintf(" %s=%d", k, n[k]); printf "%d fds:%s", t, s }' <<<"$hits")
    maps=$(cat "/proc/$pid/maps" 2>/dev/null || true)
    flags=""
    if grep -q 'libcuda\.so' <<<"$maps"; then flags+=" libcuda"; fi
    if grep -q 'libnvidia-ml\.so' <<<"$maps"; then flags+=" libnvidia-ml"; fi
    if grep -qx "/proc/$pid/fd /dev/nvidia-uvm" <<<"$hits"; then flags+=" uvm"; fi
    HOLDER_PIDS+=("$pid")
    HOLDER_LINES+=("$(printf '%8s  %-16s %s%s' "$pid" "$comm" "$line" "${flags:+  [${flags# }]}")")
  done < <(awk '{ sub("^/proc/", "", $1); sub("/fd$", "", $1); print $1 }' <<<"$hits" | sort -un)
}

print_holders() {
  local l
  echo "Processes holding /dev/nvidia* (readable /proc/<pid>/fd only):"
  if ((${#HOLDER_LINES[@]})); then
    for l in "${HOLDER_LINES[@]}"; do echo "$l"; done
    echo "  flags: libcuda = CUDA driver mapped, libnvidia-ml = NVML mapped, uvm = /dev/nvidia-uvm open (cuInit)"
  else
    echo "  none"
  fi
  echo "Unreadable /proc/<pid>/fd (other users' userspace processes, needs root): $UNREADABLE_COUNT"
  if ((${#UNREADABLE_NVIDIA[@]})); then
    echo "  NVIDIA-named among them (fds not inspectable): ${UNREADABLE_NVIDIA[*]}"
  fi
}

daemon_running() {
  local c
  for c in /proc/[0-9]*/comm; do
    [[ $(cat "$c" 2>/dev/null || true) == "$1" ]] && return 0
  done
  return 1
}

print_daemons() {
  local name
  for name in nvidia-persistenced nvidia-powerd; do
    if daemon_running "$name"; then
      echo "$name: running"
    else
      echo "$name: not running"
    fi
  done
}

# Connected + enabled connectors on the dGPU's DRM card (sysfs status is the
# cached state; reading it does not trigger a probe).
active_connectors() {
  local card s conn out=()
  for card in "$dev_dir"/drm/card*; do
    [[ -d $card ]] || continue
    for s in /sys/class/drm/"$(basename "$card")"-*/status; do
      [[ -r $s ]] || continue
      conn=$(basename "$(dirname "$s")")
      if [[ $(<"$s") == connected && $(cat "$(dirname "$s")/enabled" 2>/dev/null || true) == enabled ]]; then
        out+=("$conn")
      fi
    done
  done
  if ((${#out[@]})); then echo "${out[*]}"; fi
}

print_pid_report() {
  local pid=$1 fds maps line n=0
  echo "--- pid $pid ---"
  if [[ ! -d /proc/$pid ]]; then
    echo "  not running"
    PID_HOLDS=0
    return
  fi
  echo "  comm: $(cat "/proc/$pid/comm" 2>/dev/null || printf '?')"
  if [[ -r /proc/$pid/fd && -x /proc/$pid/fd ]]; then
    fds=$(find "/proc/$pid/fd" -maxdepth 1 -lname '/dev/nvidia*' -printf '%l\n' 2>/dev/null | sort | uniq -c || true)
    if [[ -n $fds ]]; then
      n=$(awk '{ t += $1 } END { print t + 0 }' <<<"$fds")
      echo "  /dev/nvidia* fds: $n"
      while IFS= read -r line; do echo "    $line"; done <<<"$fds"
    else
      echo "  /dev/nvidia* fds: 0"
    fi
  else
    echo "  /proc/$pid/fd unreadable (other user)"
  fi
  maps=$(cat "/proc/$pid/maps" 2>/dev/null || true)
  if [[ -z $maps ]]; then
    echo "  maps: unreadable"
  else
    local lib
    for lib in libcuda.so libnvidia-ml.so libnvidia-gpucomp.so libnvcuvid.so libnvidia-encode.so; do
      if grep -q "${lib//./\\.}" <<<"$maps"; then
        echo "  maps $lib: yes"
      else
        echo "  maps $lib: no"
      fi
    done
  fi
  if ((n > 0)); then PID_HOLDS=1; else PID_HOLDS=0; fi
}

# --- report ----------------------------------------------------------------

vendor=$(<"$dev_dir/vendor")
devid=$(<"$dev_dir/device")
driver=$(basename "$(readlink "$dev_dir/driver" 2>/dev/null || printf 'none')")
echo "dGPU: $device  [$vendor:$devid, driver $driver]"
echo "power/control:  $(read_pm control)"
status=$(read_pm runtime_status)
echo "runtime_status: $status"
procpower=/proc/driver/nvidia/gpus/$device/power
if [[ -r $procpower ]]; then
  grep -E '^(Runtime D3 status|Video Memory):' "$procpower" | sed 's/^/driver: /' || true
fi
conns=$(active_connectors)
if [[ -n $conns ]]; then
  echo "dGPU outputs connected+enabled: $conns  (a lit output keeps the dGPU awake)"
else
  echo "dGPU outputs connected+enabled: none"
fi
print_daemons

susp0=$(read_pm runtime_suspended_time)
act0=$(read_pm runtime_active_time)
t0=$SECONDS

rc=0
if [[ -n $wait_secs ]]; then
  echo "Waiting up to ${wait_secs}s for runtime_status=suspended (poll every ${interval}s, sysfs only)..."
  reached=0
  while :; do
    status=$(read_pm runtime_status)
    if [[ $status == suspended ]]; then
      reached=1
      echo "runtime_status=suspended after $((SECONDS - t0))s"
      break
    fi
    ((SECONDS - t0 + interval <= wait_secs)) || break
    sleep "$interval"
  done
  ((reached)) || {
    rc=1
    echo "runtime_status still '$status' after ${wait_secs}s"
  }
else
  sleep "$sample"
fi

susp1=$(read_pm runtime_suspended_time)
act1=$(read_pm runtime_active_time)
elapsed=$((SECONDS - t0))
if is_uint "$susp0" && is_uint "$susp1" && is_uint "$act0" && is_uint "$act1"; then
  echo "over ${elapsed}s: runtime_suspended_time +$((susp1 - susp0)) ms, runtime_active_time +$((act1 - act0)) ms"
  echo "since boot: runtime_suspended_time ${susp1} ms, runtime_active_time ${act1} ms"
fi
echo "runtime_status now: $(read_pm runtime_status)"

scan_holders
print_holders

if [[ -n $target_pid ]]; then
  PID_HOLDS=0
  print_pid_report "$target_pid"
  if ((PID_HOLDS)); then
    echo "  => pid $target_pid HOLDS /dev/nvidia* handles"
    ((rc)) || rc=3
  else
    echo "  => pid $target_pid holds no /dev/nvidia* handle"
  fi
fi

if ((rc == 1)); then
  echo "Not suspended. Likely reasons:"
  [[ $(read_pm control) == auto ]] || echo "  - power/control is '$(read_pm control)', not 'auto' (runtime PM disabled)"
  [[ -z $conns ]] || echo "  - dGPU display outputs lit: $conns"
  if ((${#HOLDER_PIDS[@]})); then
    echo "  - /dev/nvidia* held by pid(s): ${HOLDER_PIDS[*]}"
  fi
  if ((${#UNREADABLE_NVIDIA[@]})); then
    echo "  - NVIDIA daemons with uninspectable fds: ${UNREADABLE_NVIDIA[*]}"
  fi
fi
exit "$rc"
