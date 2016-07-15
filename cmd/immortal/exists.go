package main

import (
	"os"
)

func exists(path string) bool {
	if _, err := os.Stat(path); os.IsNotExist(err) {
		return false
	}
	return true
}

func is_exec(path string) (bool, error) {
	if f, err := os.Stat(path); err != nil {
		if os.IsNotExist(err) {
			return false, nil
		} else {
			return false, err
		}
	} else if m := f.Mode(); !m.IsDir() && m&0111 != 0 {
		return true, nil
	} else {
		return false, nil
	}
}
