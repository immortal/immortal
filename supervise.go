package immortal

import (
	"time"
)

type Supervisor interface {
	Supervise()
}

type Sup struct{}

func (self Sup) Supervise() {
	println("sleeping.....")
	time.Sleep(10000 * time.Second)
}
