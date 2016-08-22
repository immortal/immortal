package immortal

import (
	"log"
	"os"
	"runtime"
	"time"
)

func (self *Sup) Info(ch <-chan os.Signal, d *Daemon) {
	for {
		select {
		case <-ch:
			status := `
Gorutines: %d
Alloc : %d
Total Alloc: %d
Sys: %d
Lookups: %d
Mallocs: %d
Frees: %d
Seconds in GC: %d
Started on: %v
Uptime: %v
Process uptime: %v
Process count: %d`
			runtime.NumGoroutine()
			s := new(runtime.MemStats)
			runtime.ReadMemStats(s)
			log.Printf(status,
				runtime.NumGoroutine(),
				s.Alloc,
				s.TotalAlloc,
				s.Sys,
				s.Lookups,
				s.Mallocs,
				s.Frees,
				s.PauseTotalNs/1000000000,
				d.start.Format(time.RFC3339),
				time.Since(d.start),
				d.Process.Uptime(),
				d.count)
		}
	}
}
