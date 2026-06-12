
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
    archive="$(mktemp -d "${TMPDIR:-/tmp}/stax-demo-corpus.XXXXXX")"; \
    echo "archive: $archive"; \
    stax record -- cargo run -p stax-target --example corpus; \
    stax save "$archive"; \
    stax open "$archive"; \
    stax threads -n 0; \
    stax top -n 20 --sort self; \
    stax flame --threshold-pct 0 -d 6; \
    stax diagnose; \
    stax compare "$archive" "$archive"
