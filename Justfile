
list:
    just --list

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
