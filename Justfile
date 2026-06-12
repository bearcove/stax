
list:
    just --list

fmt:
    cargo fmt

check-target:
    cargo check -p stax-target --all-targets --message-format=short

test-target:
    cargo nextest list -p stax-target --all-targets
    cargo nextest run -p stax-target --all-targets

check-cli:
    cargo check -p stax --all-targets --message-format=short

test-cli-target-lanes:
    cargo nextest list -p stax --all-targets -E 'test(threads_output_keeps_target_lanes_past_limit)'
    cargo nextest run -p stax --all-targets -E 'test(threads_output_keeps_target_lanes_past_limit)'

docs:
    ddc build

frontend-check:
    pnpm --dir frontend typecheck
    pnpm --dir frontend build

target-span-check: fmt check-target test-target check-cli test-cli-target-lanes docs frontend-check

install:
    cargo xtask install
    sudo -n /usr/local/sbin/stax-agent setup --yes

demo-corpus:
    stax record -- cargo run -p stax-target --example corpus
    stax threads -n 0
    stax diagnose

archive-smoke:
    #!/usr/bin/env bash
    set -euo pipefail
    socket="$(mktemp -u "${TMPDIR:-/tmp}/stax-server.XXXXXX.sock")"
    archive="$(mktemp -d "${TMPDIR:-/tmp}/stax-demo-corpus.XXXXXX")"
    echo "archive: $archive"
    STAX_SERVER_SOCKET="$socket" STAX_SERVER_WS_BIND=127.0.0.1:0 cargo run -q -p stax-server &
    server_pid=$!
    cleanup() {
        kill "$server_pid" 2>/dev/null || true
        wait "$server_pid" 2>/dev/null || true
        rm -f "$socket"
    }
    trap cleanup EXIT
    for _ in {1..200}; do
        if [[ -S "$socket" ]]; then
            break
        fi
        sleep 0.05
    done
    if [[ ! -S "$socket" ]]; then
        echo "stax-server socket did not appear: $socket" >&2
        exit 1
    fi
    STAX_SERVER_SOCKET="$socket" cargo run -q -p stax -- record -- cargo run -q -p stax-target --example corpus
    STAX_SERVER_SOCKET="$socket" cargo run -q -p stax -- save "$archive"
    ls -1 "$archive"
    STAX_SERVER_SOCKET="$socket" cargo run -q -p stax -- open "$archive"
    STAX_SERVER_SOCKET="$socket" cargo run -q -p stax -- threads -n 20
    STAX_SERVER_SOCKET="$socket" cargo run -q -p stax -- top -n 20 --sort self
    STAX_SERVER_SOCKET="$socket" cargo run -q -p stax -- flame --threshold-pct 2 -d 4
    STAX_SERVER_SOCKET="$socket" cargo run -q -p stax -- diagnose
    STAX_SERVER_SOCKET="$socket" cargo run -q -p stax -- compare "$archive" "$archive"
