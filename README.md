# Immortal ⭕

[![Build Status](https://travis-ci.org/immortal/immortal.svg?branch=develop)](https://travis-ci.org/immortal/immortal)
[![Coverage Status](https://coveralls.io/repos/github/immortal/immortal/badge.svg?branch=develop)](https://coveralls.io/github/immortal/immortal?branch=develop)
[![Go Report Card](https://goreportcard.com/badge/github.com/immortal/immortal)](https://goreportcard.com/report/github.com/immortal/immortal)

A *nix cross-platform (OS agnostic) supervisor

https://immortal.run/

[ ![Download](https://api.bintray.com/packages/nbari/immortal/immortal/images/download.svg) ](https://bintray.com/nbari/immortal/immortal/_latestVersion)

# Paths

When using immortaldir:

    /usr/local/etc/immortal
    |--api1.yml
    |--api2.yml
    `--api3.yml

The name of the `file.yml` will be used to reference the service to be daemonized.

## /var/run/immortal/<name>

    /var/run/immortal
    |--api1
    |  |-lock
    |  `-immortal.sock
    |--api2
    |  |-lock
    |  `-immortal.sock
    `--api3
       |-lock
       `-immortal.sock


## immortal like non-root user

Any service launched like not using using ``immortaldir`` will follow this
structure:

    ~/.immortal
    |--(pid)
    |  `--supervise
    |     `--immortal.sock
    |--(pid)
    |  `--supervise
    |     `--immortal.sock
    `--(pid)
       `--supervise
          `--immortal.sock

# immortalctl

Will print current status and allow to manage the services

# debug

    pgrep -fl "immortal -ctl"  | awk '{print $1}' | xargs watch -n .1 pstree -p

# Test status using curl

status:

    curl --unix-socket immortal.sock http:/status -s | jq

down:

    curl --unix-socket immortal.sock http://im/signal/d -s | jq

up:

    curl --unix-socket immortal.sock http://im/signal/u -s | jq
