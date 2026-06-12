#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

PWCLI="${PWCLI:-${CODEX_HOME:-$HOME/.codex}/skills/playwright/scripts/playwright_cli.sh}"
if ! command -v npx >/dev/null 2>&1; then
    echo "web-target-smoke needs npx so the Playwright CLI wrapper can run" >&2
    exit 1
fi
if [[ ! -x "$PWCLI" ]]; then
    echo "Playwright CLI wrapper not found: $PWCLI" >&2
    exit 1
fi

socket="$(mktemp -u "${TMPDIR:-/tmp}/stax-web-smoke.XXXXXX.sock")"
archive="$(mktemp -d "${TMPDIR:-/tmp}/stax-web-smoke-archive.XXXXXX")"
ws_port="${STAX_WEB_SMOKE_WS_PORT:-18082}"
vite_port="${STAX_WEB_SMOKE_VITE_PORT:-5177}"
session="stax-web-smoke-$$"
vite_log="${TMPDIR:-/tmp}/stax-web-smoke-vite-${vite_port}.log"
server_pid=""
vite_pid=""

cleanup() {
    PLAYWRIGHT_CLI_SESSION="$session" "$PWCLI" close >/dev/null 2>&1 || true
    if [[ -n "$vite_pid" ]]; then
        kill "$vite_pid" 2>/dev/null || true
        wait "$vite_pid" 2>/dev/null || true
    fi
    if [[ -n "$server_pid" ]]; then
        kill "$server_pid" 2>/dev/null || true
        wait "$server_pid" 2>/dev/null || true
    fi
    rm -f "$socket"
}
trap cleanup EXIT

wait_for_socket() {
    for _ in {1..200}; do
        if [[ -S "$socket" ]]; then
            return 0
        fi
        sleep 0.05
    done
    echo "stax-server socket did not appear: $socket" >&2
    return 1
}

wait_for_http() {
    local url="$1"
    for _ in {1..200}; do
        if curl -fsS "$url" >/dev/null 2>&1; then
            return 0
        fi
        sleep 0.05
    done
    echo "Vite did not answer at $url" >&2
    if [[ -f "$vite_log" ]]; then
        cat "$vite_log" >&2
    fi
    return 1
}

pw() {
    PLAYWRIGHT_CLI_SESSION="$session" "$PWCLI" "$@"
}

pw_eval() {
    local output
    if ! output="$(pw eval "$1" 2>&1)"; then
        echo "$output" >&2
        return 1
    fi
    if [[ "$output" == *"### Error"* ]]; then
        echo "$output" >&2
        return 1
    fi
    printf '%s\n' "$output"
}

wait_for_eval_true() {
    local expr="$1"
    local label="$2"
    local output=""
    for _ in {1..150}; do
        output="$(pw_eval "$expr")"
        if [[ "$output" == *"### Result"* && "$output" == *"true"* ]]; then
            return 0
        fi
        sleep 0.1
    done
    echo "Timed out waiting for browser condition: $label" >&2
    echo "$output" >&2
    return 1
}

require_eval_true() {
    local expr="$1"
    local label="$2"
    local output
    output="$(pw_eval "$expr")"
    if [[ "$output" == *"### Result"* && "$output" == *"true"* ]]; then
        return 0
    fi
    echo "Browser condition failed: $label" >&2
    echo "$output" >&2
    return 1
}

dump_browser_state() {
    pw_eval "(() => { const raw = document.body.innerText; const text = raw.toLowerCase(); return JSON.stringify({ width: window.innerWidth, scrollWidth: document.documentElement.scrollWidth, overflow: document.documentElement.scrollWidth - window.innerWidth, selectedTab: document.querySelector('[role=tab][aria-selected=true]')?.textContent?.trim() ?? null, hasTopTargetWork: text.includes('top target work'), hasRecentTargetSpans: text.includes('recent target spans'), hasCollectCompletion: text.includes('corpus collect completion'), hasCopyKernel: text.includes('corpus copy kernel'), buttons: [...document.querySelectorAll('button')].map((b) => b.textContent.trim()).filter(Boolean).slice(0, 80), textStart: raw.slice(0, 3000) }); })()" >&2 || true
}

echo "archive: $archive"
STAX_SERVER_SOCKET="$socket" \
    STAX_SERVER_WS_BIND="127.0.0.1:${ws_port}" \
    cargo run -q -p stax-server &
server_pid=$!
wait_for_socket

STAX_SERVER_SOCKET="$socket" \
    cargo run -q -p stax -- record -- cargo run -q -p stax-target --example corpus
STAX_SERVER_SOCKET="$socket" cargo run -q -p stax -- save "$archive"
STAX_SERVER_SOCKET="$socket" cargo run -q -p stax -- open "$archive"

pnpm --dir frontend exec vite --host 127.0.0.1 --port "$vite_port" --strictPort \
    >"$vite_log" 2>&1 &
vite_pid=$!
wait_for_http "http://127.0.0.1:${vite_port}/"

url="http://127.0.0.1:${vite_port}/?ws=ws://127.0.0.1:${ws_port}"
pw open "$url" >/dev/null
pw resize 1280 900 >/dev/null
pw snapshot >/dev/null
wait_for_eval_true "(() => { const text = document.body.innerText; return text.includes('corpus executor') && text.includes('corpus gpu') && text.includes('target'); })()" "target lanes visible"
require_eval_true "(() => { const button = (name) => [...document.querySelectorAll('button')].find((b) => b.textContent.trim() === name); const target = button('target'); const spans = button('target spans'); if (!target || !spans) return false; target.click(); spans.click(); return true; })()" "target mode and target-spans tab controls exist"
if ! wait_for_eval_true "(() => { const text = document.body.innerText.toLowerCase(); const overflow = document.documentElement.scrollWidth - window.innerWidth; return text.includes('top target work') && text.includes('recent target spans') && text.includes('corpus collect completion') && text.includes('corpus copy kernel') && overflow <= 2; })()" "desktop target-span detail visible without overflow"; then
    dump_browser_state
    exit 1
fi
pw resize 390 844 >/dev/null
if ! wait_for_eval_true "(() => { const text = document.body.innerText; const overflow = document.documentElement.scrollWidth - window.innerWidth; return text.includes('target spans') && text.includes('corpus executor') && text.includes('corpus gpu') && overflow <= 4; })()" "mobile target UI visible without overflow"; then
    dump_browser_state
    exit 1
fi

echo "PASS: web target smoke rendered target lanes and target-span details"
