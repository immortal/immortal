package immortal

import (
	"os/user"
)

type User interface {
	Lookup(user string) (*user.User, error)
}

type iUser struct{}

func (self *iUser) Lookup(u string) (*user.User, error) {
	usr, err := user.Lookup(u)
	if err != nil {
		return nil, err
	}
	return usr, nil
}
