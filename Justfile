
list:
    just --list

install:
    cargo xtask install
    sudo -n /usr/local/sbin/stax-agent setup --yes

run:
    just stax-mac-app/install

run-release:
    just stax-mac-app/install-release

sample: install run
    stax record -F 900 --correlate-kperf --correlate-frequency 900 --pid $(pgrep -f bee.app)

sample-release: install run-release
    stax record -F 900 --correlate-kperf --correlate-frequency 900 --pid $(pgrep -f bee.app)
