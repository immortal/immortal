package immortal

import (
	"strconv"
)

const (
	logo = "2B55"
)

func Icon(h string) rune {
	i, e := strconv.ParseInt(h, 16, 32)
	if e != nil {
		return 0
	}
	return rune(i)
}
