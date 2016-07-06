package immortal

import (
	"fmt"
	"strconv"
)

const (
	logo   = "2B55"
	escape = "\x1b"
)

func Red(s string) string {
	return fmt.Sprintf("%s[0;31m%s%s[0;00m", escape, s, escape)
}

func Green(s string) string {
	return fmt.Sprintf("%s[0;32m%s%s[0;00m", escape, s, escape)
}

func Yellow(s string) string {
	return fmt.Sprintf("%s[0;33m%s%s[0;00m", escape, s, escape)
}

// Icon Unicode Hex to string
func Icon(h string) rune {
	i, e := strconv.ParseInt(h, 16, 32)
	if e != nil {
		return 0
	}
	return rune(i)
}
