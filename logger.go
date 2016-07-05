package immortal

import (
	"fmt"
	"time"
)

func Log(s string) {
	t := time.Now().UTC().Format(time.RFC3339Nano)
	fmt.Printf("%s %s\n", t, s)
}
