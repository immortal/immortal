package immortal

import "fmt"

const escape = "\x1b"

// Red return string in red
func Red(s string) string {
	return fmt.Sprintf("%s[0;31m%s%s[0;00m", escape, s, escape)
}

// Green return string in green
func Green(s string) string {
	return fmt.Sprintf("%s[0;32m%s%s[0;00m", escape, s, escape)
}

// Yellow return string in yellow
func Yellow(s string) string {
	return fmt.Sprintf("%s[0;33m%s%s[0;00m", escape, s, escape)
}
