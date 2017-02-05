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
    |--api1
    |  |--env
    |  `--run.yml
    |--api2
    |  |--env
    |  `--run.yml
    `--api3.yml

If using a directory the name of the directory will be used to reference the
application to be daemonized only if within the directory exists a proper
`run.yml` file

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

Considering the use of dir `/var/run/immortal/app/` when using `immortal-dir`

## immortal like non-root user

When running like non-root user or not by ``immortal-dir`` there will be no lock
so command can be run multiple times:

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



# debug

    pgrep -fl "immortal -ctl"  | awk '{print $1}' | xargs watch -n .1 pstree -p

# Test status using curl

    curl --unix-socket immortal.sock http:/ -s | jq
