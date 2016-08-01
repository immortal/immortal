package immortal

type Watcher interface {
	WatchPid(pid int)
}

type Watch struct{}

func (self *Watch) WatchPid(pid int) {
}
