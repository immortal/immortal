package immortal

import (
	"log"
	"os"
	"runtime"
	"time"
)

func (d *Daemon) Info(ch <-chan os.Signal) {
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
Process count: %d`
			runtime.NumGoroutine()
			r := new(runtime.MemStats)
			runtime.ReadMemStats(r)
			log.Printf(status,
				runtime.NumGoroutine(),
				r.Alloc,
				r.TotalAlloc,
				r.Sys,
				r.Lookups,
				r.Mallocs,
				r.Frees,
				r.PauseTotalNs/1000000000,
				d.sTime.Format(time.RFC3339),
				time.Since(d.sTime),
				d.count)
		}
	}
}
