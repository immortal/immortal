package immortal

import (
	"fmt"
	"os/user"
)

type UserFinder interface {
	Lookup(user string) (*user.User, error)
}

type User struct{}

func (self *User) Lookup(u string) (*user.User, error) {
	usr, err := user.Lookup(u)
	if err != nil {
		if _, ok := err.(user.UnknownUserError); ok {
			return nil, fmt.Errorf("User %q does not exist.", u)
		} else if err != nil {
			return nil, fmt.Errorf("Error looking up user: %q", u)
		}
	}
	return usr, nil
}
