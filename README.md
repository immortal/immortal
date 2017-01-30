# Immortal ⭕

[![Build Status](https://travis-ci.org/immortal/immortal.svg?branch=develop)](https://travis-ci.org/immortal/immortal)
[![Coverage Status](https://coveralls.io/repos/github/immortal/immortal/badge.svg?branch=develop)](https://coveralls.io/github/immortal/immortal?branch=develop)
[![Go Report Card](https://goreportcard.com/badge/github.com/immortal/immortal)](https://goreportcard.com/report/github.com/immortal/immortal)

A *nix cross-platform (OS agnostic) supervisor

https://immortal.run/

[ ![Download](https://api.bintray.com/packages/nbari/immortal/immortal/images/download.svg) ](https://bintray.com/nbari/immortal/immortal/_latestVersion)

# Paths

When using immortal-dir:

    /usr/local/etc/immortal
    |--api1.example.com
    |  |--env
    |  |--run.yml
    |  `--supervice
    |     |--lock
    |     `--immortal.sock
    |--api2.example.com
    |  |--env
    |  |--run.yml
    |  `--supervice
    |     |--lock
    |     `--immortal.sock
    `--api3.example.com
       |--env
       |--run.yml
       `--supervice
          |--lock
           `--immortal.sock

When running like non-root user or not by ``immortal-dir`` there will be no lock
so command can be run multiple times:

    ~/.immortal
    |--(hash)
    |  `--supervise
    |     `--immortal.sock
    |--(hash)
    |  `--supervise
    |     `--immortal.sock
    `--(hash)
       `--supervise
          `--immortal.sock


# debug

    pgrep -fl "immortal -ctl"  | awk '{print $1}' | xargs watch -n .1 pstree -p

# Test status using curl

    curl --unix-socket immortal.sock http:/ -s | jq
