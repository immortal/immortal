package immortal

import (
	"fmt"
	"os"
	"time"
)

func Log(s interface{}) {
	t := time.Now().UTC().Format(time.RFC3339Nano)
	log := fmt.Sprintf("%s %v\n", t, s)

	f, err := os.OpenFile("/tmp/immortal.log", os.O_APPEND|os.O_WRONLY, 0600)
	if err != nil {
		panic(err)
	}

	defer f.Close()

	if _, err = f.WriteString(log); err != nil {
		panic(err)
	}
}
