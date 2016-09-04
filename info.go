package immortal

import (
	"log"
	"os"
	"runtime"
	"time"
)

// Info log current daemon status after receiving a QUIT signal "kill -3 PID"
func (d *Daemon) Info() {
	var m runtime.MemStats
	runtime.ReadMemStats(&m)
	status := `PID: %d
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
	log.Printf(status,
		os.Getpid(),
		runtime.NumGoroutine(),
		m.Alloc,
		m.TotalAlloc,
		m.Sys,
		m.Lookups,
		m.Mallocs,
		m.Frees,
		m.PauseTotalNs/1000000000,
		d.sTime.Format(time.RFC3339),
		time.Since(d.sTime),
		d.count)
}
