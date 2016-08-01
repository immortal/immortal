package immortal

type Supervisor interface {
	Supervise()
}

type Sup struct{}

func (self Sup) Supervise() {
	println("0000")
}
