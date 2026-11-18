#!/bin/sh
# Starts the app on X11 (Xvfb) and on Wayland (headless Weston) and checks that
# it is still running after a few seconds, i.e. the window opened without a crash.
#
# Usage: scripts/smoke-test.sh path/to/dancesonic-studio
set -eu

bin=${1:?usage: $0 path/to/dancesonic-studio}
seconds=${SMOKE_SECONDS:-5}

# Headless displays have no GPU; software rendering avoids Vulkan/EGL noise and flakiness.
export GSK_RENDERER=cairo

# timeout(1) exits with 124 when it had to stop a program that was still running:
# that is the success case here.
expect_running() {
    status=0
    "$@" || status=$?
    if [ "$status" -ne 124 ]; then
        echo "app exited early with status $status" >&2
        exit 1
    fi
}

echo "X11 (Xvfb)"
expect_running env GDK_BACKEND=x11 xvfb-run -a timeout "$seconds" "$bin"

echo "Wayland (headless Weston)"
runtime=$(mktemp -d)
chmod 700 "$runtime"
XDG_RUNTIME_DIR=$runtime weston --backend=headless --socket=smoke-wayland --idle-time=0 &
weston_pid=$!
trap 'kill "$weston_pid" 2>/dev/null || true; rm -rf "$runtime"' EXIT
i=0
until [ -S "$runtime/smoke-wayland" ]; do
    i=$((i + 1))
    if [ "$i" -gt 50 ]; then
        echo "weston did not start" >&2
        exit 1
    fi
    sleep 0.1
done
expect_running env -u DISPLAY XDG_RUNTIME_DIR="$runtime" WAYLAND_DISPLAY=smoke-wayland \
    GDK_BACKEND=wayland timeout "$seconds" "$bin"

echo "ok"
