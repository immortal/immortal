package immortal

import (
	"testing"
)

func (self *Parse) exists(path string) bool {
	if _, err := os.Stat(path); os.IsNotExist(err) {
		return false
	}
	return true
}

type MyParser struct {
	Flags
}

func (self *MyParser) Parse() *Flags {
	self.Flags.Logfile = "mock-test"
	return &self.Flags
}

func (self *MyParser) exists(path string) bool {
}
