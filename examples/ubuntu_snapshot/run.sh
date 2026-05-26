#!/bin/sh
set -eu

if [ "$#" -lt 1 ]; then
    echo "usage: $0 <command> [args...]" >&2
    exit 2
fi

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
examples_dir=$(CDPATH= cd -- "$script_dir/.." && pwd)
repo_dir=$(CDPATH= cd -- "$examples_dir/.." && pwd)
binary="$examples_dir/target/debug/ubuntu_snapshot"
snapshot_dir="${HOME:?}/.libkrun/snapshot"
snapshot_metadata="ubuntu-snapshot-v8-8gib-ram"
run_dir="${HOME:?}/.libkrun/run"
saved_stty=

if [ -t 1 ]; then
    saved_stty=$(stty -g)
    trap 'stty "$saved_stty" 2>/dev/null || true' EXIT HUP INT TERM
fi

cd "$examples_dir"

mkdir -p "$run_dir"
lock_file="$run_dir/ubuntu_snapshot.lock"
if [ -f "$lock_file" ]; then
    rm -f "$lock_file"
fi
if ! mkdir "$lock_file" 2>/dev/null; then
    echo "another ubuntu_snapshot command is still running" >&2
    exit 1
fi
trap 'rm -rf "$lock_file"; if [ -n "$saved_stty" ]; then stty "$saved_stty" 2>/dev/null || true; fi' EXIT HUP INT TERM

if [ ! -x "$binary" ]; then
    echo "missing $binary; build it first with: LIBRARY_PATH=$repo_dir/target/debug cargo build -p ubuntu_snapshot" >&2
    exit 1
fi

if [ "$script_dir/src/main.rs" -nt "$binary" ] || [ "$script_dir/Cargo.toml" -nt "$binary" ]; then
    echo "stale $binary; rebuild it first with: cd $examples_dir && LIBRARY_PATH=$repo_dir/target/debug cargo build -p ubuntu_snapshot" >&2
    exit 1
fi

if [ -e "$snapshot_dir/vmstate.bin" ] || [ -e "$snapshot_dir/pages.img" ]; then
    if [ ! -f "$snapshot_dir/metadata" ] || [ "$(cat "$snapshot_dir/metadata")" != "$snapshot_metadata" ]; then
        rm -rf "$snapshot_dir"
    fi
fi

codesign --force --sign - \
    --entitlements "$examples_dir/chroot_vm.entitlements" \
    --timestamp=none \
    "$binary" >/dev/null 2>&1

stdout_file="$run_dir/ubuntu_snapshot.stdout"
stderr_file="$run_dir/ubuntu_snapshot.stderr"

run_vm() {
    DYLD_LIBRARY_PATH="$repo_dir/target/debug:$repo_dir/../libkrunfw${DYLD_LIBRARY_PATH:+:$DYLD_LIBRARY_PATH}" \
        "$binary" "$@" >"$stdout_file" 2>"$stderr_file"
}

if run_vm "$@"; then
    if [ -n "$saved_stty" ]; then
        stty "$saved_stty" 2>/dev/null || true
    fi
    cat "$stdout_file"
    cat "$stderr_file" >&2
    exit 0
fi

stderr_text=$(cat "$stderr_file")
case "$stderr_text" in
    *"snapshot restore failed"*|*"queue count mismatch"*)
        rm -rf "$snapshot_dir"
        if run_vm "$@"; then
            if [ -n "$saved_stty" ]; then
                stty "$saved_stty" 2>/dev/null || true
            fi
            cat "$stdout_file"
            cat "$stderr_file" >&2
            exit 0
        fi
        ;;
esac

if [ -n "$saved_stty" ]; then
    stty "$saved_stty" 2>/dev/null || true
fi
cat "$stdout_file"
cat "$stderr_file" >&2
exit 1
