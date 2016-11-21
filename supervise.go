package immortal

import (
	"log"
	"os"
	"os/signal"
	"sync/atomic"
	"syscall"
	"time"
)

// Supervise keep daemon process up and running
func Supervise(d *Daemon) {
	var (
		err  error
		info = make(chan os.Signal)
		p    *process
		pid  int
		wait time.Duration
	)

	// start a new process
	p, err = d.Run(NewProcess(d.cfg))
	if err != nil {
		log.Fatal(err)
	}

	// Info loop, kill -3 PPID get stats
	signal.Notify(info, syscall.SIGQUIT)

	for {
		select {
		case <-d.quit:
			return
		case <-info:
			d.Info()
		case <-d.run:
			time.Sleep(wait)
			if lock := atomic.LoadUint32(&d.lock); lock == 0 {
				// create a new process
				np := NewProcess(d.cfg)
				if p, err = d.Run(np); err != nil {
					close(np.quit)
					log.Print(err)
					wait = time.Second
					d.run <- struct{}{}
				}
			}
		case err := <-p.errch:
			// unlock, or lock once
			atomic.StoreUint32(&d.lock, d.lockOnce)
			if err != nil && err.Error() == "EXIT" {
				log.Printf("PID: %d Exited", pid)
			} else {
				log.Printf("PID %d terminated, %s [%v user  %v sys  %s up]\n",
					p.cmd.ProcessState.Pid(),
					p.cmd.ProcessState,
					p.cmd.ProcessState.UserTime(),
					p.cmd.ProcessState.SystemTime(),
					time.Since(p.sTime),
				)
				// calculate time for next reboot (avoids high CPU usage)
				uptime := p.eTime.Sub(p.sTime)
				wait = 0 * time.Second
				if uptime < time.Second {
					wait = time.Second - uptime
				}
			}
			// follow the new pid and stop running the command
			// unless the new pid dies
			if d.cfg.Pid.Follow != "" {
				pid, err = d.ReadPidFile(d.cfg.Pid.Follow)
				if err != nil {
					log.Printf("Cannot read pidfile:%s, %s", d.cfg.Pid.Follow, err)
					d.run <- struct{}{}
				} else {
					// check if pid in file is valid
					if pid > 1 && pid != p.Pid() && d.IsRunning(pid) {
						log.Printf("Watching pid %d on file: %s", pid, d.cfg.Pid.Follow)
						d.WatchPid(pid, p.errch)
					} else {
						// if cmd exits or process is kill
						d.run <- struct{}{}
					}
				}
			} else {
				d.run <- struct{}{}
			}
		}
	}
}
