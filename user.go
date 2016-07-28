package immortal

import (
	"os/user"
)

type UserI interface {
	Lookup(u string) (*user.User, error)
}

type Users struct{}

func (self *Users) Lookup(u string) (*user.User, error) {
	usr, err := user.Lookup(u)
	if err != nil {
		return nil, err
	}
	return usr, nil
}
