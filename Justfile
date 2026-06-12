
list:
    just --list

fmt:
    cargo fmt

fmt-check:
    cargo fmt --check

diff-check:
    git diff --check

check-target:
    cargo check -p stax-target --all-targets --message-format=short

check-live-proto:
    cargo check -p stax-live-proto --all-targets --message-format=short

check-live:
    cargo check -p stax-live --all-targets --message-format=short

check-server:
    cargo check -p stax-server --all-targets --message-format=short

check-mac-kperf-parse:
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ "$(uname -s)" == "Darwin" ]]; then
        cargo check -p stax-mac-kperf-parse --all-targets --message-format=short
    else
        echo "skipping stax-mac-kperf-parse check on non-macOS host"
    fi

test-target:
    cargo nextest list -p stax-target --all-targets
    cargo nextest run -p stax-target --all-targets

check-cli:
    cargo check -p stax --all-targets --message-format=short

test-cli-target-lanes:
    cargo nextest list -p stax --all-targets -E 'test(threads_output_keeps_target_lanes_past_limit)'
    cargo nextest run -p stax --all-targets -E 'test(threads_output_keeps_target_lanes_past_limit)'

test-cli-compare-json:
    cargo nextest list -p stax --all-targets -E 'test(compare_report_serializes_machine_readable_deltas)'
    cargo nextest run -p stax --all-targets -E 'test(compare_report_serializes_machine_readable_deltas)'

test-server-target-ingest:
    cargo nextest list -p stax-server --all-targets -E 'test(ingest_links_spans_to_origin_cpu_stack)'
    cargo nextest run -p stax-server --all-targets -E 'test(ingest_links_spans_to_origin_cpu_stack)'

test-mac-kperf-timebase:
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ "$(uname -s)" == "Darwin" ]]; then
        cargo nextest list -p stax-mac-kperf-parse --all-targets -E 'test(mach_timebase_masks_cpu_bits_before_ns_conversion)'
        cargo nextest run -p stax-mac-kperf-parse --all-targets -E 'test(mach_timebase_masks_cpu_bits_before_ns_conversion)'
    else
        echo "skipping stax-mac-kperf-parse timebase test on non-macOS host"
    fi

docs:
    ddc build

frontend-check:
    pnpm --dir frontend typecheck
    pnpm --dir frontend build

web-target-smoke:
    bash frontend/scripts/web-target-smoke.sh

target-span-check: fmt-check check-target check-live-proto check-live check-server check-mac-kperf-parse test-target test-cli-target-lanes test-cli-compare-json test-server-target-ingest test-mac-kperf-timebase check-cli docs frontend-check diff-check

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
    socket_dir="$(mktemp -d "${TMPDIR:-/tmp}/stax-server.XXXXXX")"
    socket="$socket_dir/server.sock"
    archive="$(mktemp -d "${TMPDIR:-/tmp}/stax-demo-corpus.XXXXXX")"
    echo "archive: $archive"
    STAX_SERVER_SOCKET="$socket" STAX_SERVER_WS_BIND=127.0.0.1:0 cargo run -q -p stax-server &
    server_pid=$!
    cleanup() {
        kill "$server_pid" 2>/dev/null || true
        wait "$server_pid" 2>/dev/null || true
        rm -rf "$socket_dir"
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
    STAX_SERVER_SOCKET="$socket" cargo run -q -p stax -- list
    STAX_SERVER_SOCKET="$socket" cargo run -q -p stax -- select-run 1
    STAX_SERVER_SOCKET="$socket" cargo run -q -p stax -- threads --run 1 -n 20
    STAX_SERVER_SOCKET="$socket" cargo run -q -p stax -- top --run 1 -n 20 --sort self
    STAX_SERVER_SOCKET="$socket" cargo run -q -p stax -- flame --run 1 --threshold-pct 2 -d 4
    STAX_SERVER_SOCKET="$socket" cargo run -q -p stax -- diagnose --run 1
    STAX_SERVER_SOCKET="$socket" cargo run -q -p stax -- compare "$archive" "$archive"
    compare_json="$(mktemp "${TMPDIR:-/tmp}/stax-compare.XXXXXX")"
    STAX_SERVER_SOCKET="$socket" cargo run -q -p stax -- compare --json "$archive" "$archive" > "$compare_json"
    ruby -rjson -e 'j=JSON.parse(File.read(ARGV[0])); abort "missing target delta" unless j.dig("metrics","target_time","delta_ns") == 0; abort "missing lanes" unless j.fetch("top_target_lanes").any?' "$compare_json"
    rm -f "$compare_json"
