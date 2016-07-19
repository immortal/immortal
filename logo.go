package immortal

import (
	"strconv"
)

const (
	logo = "2B55"
)

func Logo() rune {
	return Icon(logo)
}

// Icon Unicode Hex to string
func Icon(h string) rune {
	i, e := strconv.ParseInt(h, 16, 32)
	if e != nil {
		return 0
	}
	return rune(i)
}
