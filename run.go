package immortal

import (
	"fmt"
)

type Runner interface {
	Run()
}

type Run struct {
}

func (self *Run) Run() {
	println("running...")

	fmt.Printf("%#v", self)
}
