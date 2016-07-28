package immortal

import (
	"os/user"
)

type UserInterface interface {
	Lookup(user string) (*user.User, error)
}

type User struct{}

func (self *User) Lookup(u string) (*user.User, error) {
	usr, err := user.Lookup(u)
	if err != nil {
		return nil, err
	}
	return usr, nil
}
