package immortal

type Forker interface {
	Fork()
}

type Fork struct{}

func (self *Fork) Fork() {
	println("forking....")
}
