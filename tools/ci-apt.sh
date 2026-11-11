#!/usr/bin/env bash
# CI: installs Ubuntu packages on GitHub's runners (ci-apt.sh <package>…).
# Their package mirror stalls now and then: a job would sit in apt-get for
# minutes, then fail, or hang until cancelled. Here each apt-get gives up on
# a connection that's gone quiet after 20 s and altogether after a few
# minutes, and is tried three times; after a failure Azure's mirror of
# Ubuntu (the x86_64 runners' default) is swapped for Ubuntu's own.
set -u

apt() {
    local limit=$1
    shift
    sudo timeout "$limit" apt-get -o Acquire::Retries=2 -o Acquire::http::Timeout=20 -o Acquire::https::Timeout=20 \
        -o DPkg::Lock::Timeout=60 "$@"
}

try() {
    local attempt
    for attempt in 1 2 3; do
        apt "$@" && return 0
        echo "::warning::apt-get ${2} didn't finish (attempt ${attempt} of 3)"
        # Stopped half-way through installing: finished off before going again.
        sudo dpkg --configure -a || true
        sudo sed -i 's|azure\.archive\.ubuntu\.com|archive.ubuntu.com|g' \
            /etc/apt/sources.list /etc/apt/sources.list.d/*.sources /etc/apt/sources.list.d/*.list 2>/dev/null || true
        sleep 5
    done
    return 1
}

try 180 update && try 480 install -y "$@"
