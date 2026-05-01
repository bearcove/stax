
list:
    just --list

install:
    cargo xtask install
    sudo -n /usr/local/sbin/stax-agent setup --yes

run:
    just stax-mac-app/install

sample: install run
    stax record -F 900 --correlate-kperf --correlate-frequency 900 --pid $(pgrep -f bee.app)
