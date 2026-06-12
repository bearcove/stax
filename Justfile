
list:
    just --list

install:
    cargo xtask install
    sudo -n /usr/local/sbin/stax-agent setup --yes

demo-corpus:
    stax record -- cargo run -p stax-target --example corpus
    stax threads -n 0
    stax diagnose
