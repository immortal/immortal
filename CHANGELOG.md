## 0.18.0

* Added option `retries`, `-r` to specify the maximum number of tries before exiting the program
* Environment `IMMORTAL_RETRIES_EXIT` if set when using `retries` and using a
configuration file, it will exit the supervisor instead of marking it as stop,
the default is like the `once` signal, but when using from command line no
`run.yml` it will exit.

## 0.17.0

* Cleaned tests (Dockerfile for linux)
* Created a Supervisor struct to handle better the supervise logic
* Give priority to environment `$HOME` instead of HomeDir from `user.Current()`
* Improved lint
* Print cmd name (not just PID) in the log when the process terminates [#29](https://github.com/immortal/immortal/pull/29), thanks @marko-turk
* Removed info.go (signal.Notify) from supervise.go
* Replaced lock/map with sync.Map in scandir.go
* Updated HandleSignal to use `GetParam` from violetear
