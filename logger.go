package immortal

import (
	"log"
	"time"
)

//func Log(l *log.Logger, msg string) {
func Log(msg string) {
	log.SetPrefix(time.Now().UTC().Format(time.RFC3339Nano) + " ")
	log.Print(msg)
}
