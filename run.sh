#!/usr/bin/env sh
# Supervisor loop for personal-agent.
#
# - Runs `cargo run`.
# - If the app exits with 255 (Rust's exit(-1) on Unix), rebuild and rerun.
# - If the app exits with 0 (graceful shutdown, e.g. Ctrl+C), stop.
# - Any other exit code is treated as an error and stops the loop.

set -u

cd "$(dirname "$0")"

# ── Mode: -d for debug, otherwise release ─────────────────────────────────────
MODE="release"
if [ "${1:-}" = "-d" ]; then
    MODE="debug"
fi

# ── Python venv setup (optional) ─────────────────────────────────────────────
# Creates .venv/ and installs requirements.txt if Python is available.
# If Python is not installed, the app starts normally but Python-based MCP
# servers (e.g. Gmail, Google Calendar) will fail to connect.
VENV_DIR=".venv"
REQUIREMENTS="requirements.txt"

if [ ! -f "$VENV_DIR/bin/python3" ]; then
    if command -v uv >/dev/null 2>&1; then
        echo "[run.sh] Setting up Python venv with uv …"
        uv venv "$VENV_DIR" && uv pip install -r "$REQUIREMENTS" \
            && echo "[run.sh] Python venv ready." \
            || echo "[run.sh] Warning: Python venv setup failed — Python MCP servers will be unavailable."
    elif command -v python3 >/dev/null 2>&1; then
        echo "[run.sh] Setting up Python venv …"
        python3 -m venv "$VENV_DIR" && "$VENV_DIR/bin/pip" install -r "$REQUIREMENTS" \
            && echo "[run.sh] Python venv ready." \
            || echo "[run.sh] Warning: Python venv setup failed — Python MCP servers will be unavailable."
    else
        echo "[run.sh] Warning: python3 not found — Python MCP servers will be unavailable."
    fi
fi

# If the venv was created, prepend it to PATH so every child process resolves
# python3 to the venv automatically (MCP servers, agent shell commands, etc.).
if [ -f "$VENV_DIR/bin/python3" ]; then
    export PATH="$(pwd)/$VENV_DIR/bin:$PATH"
fi

export TS_RS_EXPERIMENT=this_is_unstable_software

while true; do
    if [ "$MODE" = "release" ]; then
        RUSTFLAGS="-A warnings" cargo run --release
    else
        RUSTFLAGS="-A warnings" cargo run
    fi
    code=$?

    if [ "$code" -eq 0 ]; then
        echo "[run.sh] App exited cleanly. Stopping."
        exit 0
    elif [ "$code" -eq 255 ]; then
        echo "[run.sh] App requested restart (exit -1). Rebuilding…"
        continue
    else
        echo "[run.sh] App exited with code $code. Stopping."
        exit "$code"
    fi
done
