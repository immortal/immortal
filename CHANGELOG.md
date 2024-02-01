## 0.24.5
* adding ability to assign names to services [#77](https://github.com/immortal/immortal/pull/77), thanks @xxxserxxx
* Fixes output piping issue [#76](https://github.com/immortal/immortal/pull/76), thanks @xxxserxxx

## 0.24.2
* Use of go modules instead of dep, thanks @chenrui333

## 0.24.1
* Support for FreeBSD/arm64, thanks @t6

## 0.24.0
* Added the `post_exit` option to call a command after process exits, passing as an argument the exit code (Requires go >= 1.12) [#54](https://github.com/immortal/immortal/issues/54), thanks @olgeni

## 0.23.0
* Implemented option `-cc` checks the config file, print the config and exits with code 0 if no error was found, or exits with code 1 an error was found

## 0.22.0
* Fixed logger `Log` not to close pipe even when Scan() fails [#46](https://github.com/immortal/immortal/pull/46), thanks @honteng

## 0.21.0
* Implemented `retries 0` defaults to `-1` run forever, if set to 0 it will just run once and exit
* Fixed supervisor to wait for the http socket server to be closed before exiting
* Using RWmutex to prevent race conditions
* Improved logger to terminate the custom logger in case doesn't exit after closing StdinPipe

## 0.20.0
* Added the `require_cmd` option that prevents starting a service based on the output of command (exit 0), thanks @luetge

## 0.19.0
* Added option `-n` no-daemon mode, stays in the foreground [#40](https://github.com/immortal/immortal/pull/40), thanks @loafoe
* Use service name derived from config when using `immortal -c service.yml` [#39](https://github.com/immortal/immortal/pull/30), thanks @loafoe

## 0.18.0
* Added option `retries`, `-r` to specify the maximum number of tries before exiting the program
* Environment `IMMORTAL_EXIT` used to exit when running immortal with a config file, helps to avoid a race condition (start/stop) when using immortaldir
* `immortalctl` prints now process that are about to start with a defined `wait` value
* Renamed option `-s` to `-w` to be more consistent with the config file option `wait`
* Signals are only sent to process then this is up and running

## 0.17.0
* Cleaned tests (Dockerfile for linux)
* Created a Supervisor struct to handle better the supervise logic
* Give priority to environment `$HOME` instead of HomeDir from `user.Current()`
* Improved lint
* Print cmd name (not just PID) in the log when the process terminates [#29](https://github.com/immortal/immortal/pull/29), thanks @marko-turk
* Removed info.go (signal.Notify) from supervise.go
* Replaced lock/map with sync.Map in scandir.go
* Updated HandleSignal to use `GetParam` from violetear
