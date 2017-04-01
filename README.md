# ⭕  immortal

[![Build Status](https://travis-ci.org/immortal/immortal.svg?branch=develop)](https://travis-ci.org/immortal/immortal)
[![Coverage Status](https://coveralls.io/repos/github/immortal/immortal/badge.svg?branch=master)](https://coveralls.io/github/immortal/immortal?branch=master)
[![codecov](https://codecov.io/gh/immortal/immortal/branch/master/graph/badge.svg)](https://codecov.io/gh/immortal/immortal)
[![Go Report Card](https://goreportcard.com/badge/github.com/immortal/immortal)](https://goreportcard.com/report/github.com/immortal/immortal)

A *nix cross-platform (OS agnostic) supervisor

https://immortal.run/

[![GitHub release](https://img.shields.io/github/release/immortal/immortal.svg)](https://github.com/immortal/immortal/releases)

If services need to run on behalf other system user `www, nobody, www-data`,
not `root`, **immortal** should be compiled from source for the desired
target/architecture, otherwise, this error may be returned:

    Error looking up user: "www". user: Lookup requires cgo

See more: https://golang.org/cmd/cgo/

If using [FreeBSD](https://github.com/freebsd/freebsd-ports/tree/master/sysutils/immortal)
or [macOS](https://github.com/immortal/homebrew-tap)
you can install using [pkg/ports](http://immortal.run/freebsd/)
or [homebrew](http://immortal.run/mac/), for other platforms work is in
progress, any help for making the port/package for other systems would be
appreciated.

## Compile from source

Setup go environment https://golang.org/doc/install

> go >= 1.7 is required

For example using $HOME/go for your workspace

    $ export GOPATH=$HOME/go

Create the directory:

    $ mkdir -p $HOME/go/src/github.com/immortal

Clone project into that directory:

    $ git clone git@github.com:immortal/immortal.git $HOME/go/src/github.com/immortal/immortal

Build by just typing make:

    $ cd $HOME/go/src/github.com/immortal/immortal
    $ make

To install/uninstall:

    $ make install
    $ make uninstall

# configuration example

Content of file `/usr/local/etc/immortal/www.yml`:

```yaml
# pkg install go-www
cmd: www
cwd: /usr/ports
log:
    file: /var/log/www.log
    age: 10  # seconds
    num: 7   # int
    size: 1  # MegaBytes
wait: 1
require:
  - foo
  - bar
```

If `foo` and `bar` are not running, the service `www` will not be started.

> `foo` and `bar` are the names for the services defined on the same path www.yaml is located, foo.yml & bar.yml

# Paths

When using immortaldir:

    /usr/local/etc/immortal
    |--foo.yml
    |--bar.yml
    `--www.yml

The name of the `file.yml` will be used to reference the service to be
daemonized excluding the extension `.yml`.:

    foo
    bar
    www

## /var/run/immortal/<name>

    /var/run/immortal
    |--foo
    |  |-lock
    |  `-immortal.sock
    |--bar
    |  |-lock
    |  `-immortal.sock
    `--www
       |-lock
       `-immortal.sock


## immortal like non-root user

Any service launched like not using using ``immortaldir`` will follow this
structure:

    ~/.immortal
    |--(pid)
    |  |--lock
    |  `--immortal.sock
    |--(pid)
    |  |--lock
    |  `--immortal.sock
    `--(pid)
       |--lock
       `--immortal.sock

# immortalctl

Will print current status and allow to manage the services

# debug

    pgrep -fl "immortal -ctl"  | awk '{print $1}' | xargs watch -n .1 pstree -p

# Test status using curl & [jq](https://stedolan.github.io/jq/)

status:

    curl --unix-socket immortal.sock http:/status -s | jq

> note the single '/' https://superuser.com/a/925610/284722


down:

    curl --unix-socket immortal.sock http://im/signal/d -s | jq

up:

    curl --unix-socket immortal.sock http://im/signal/u -s | jq
