package immortal

import (
	"fmt"
	"io/ioutil"
	"time"
)

func Log(s string) {
	t := time.Now().UTC().Format(time.RFC3339Nano)
	log := fmt.Sprintf("%s %s\n", t, s)
	_ = ioutil.WriteFile("/tmp/immortal.log", []byte(log), 0644)
}
