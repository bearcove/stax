
list:
    just --list

install:
    cargo xtask install
    sudo -n /usr/local/sbin/stax-agent setup --yes
