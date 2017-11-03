package immortal

import (
	"log"
	"sync/atomic"
	"time"
)

// Supervise keep daemon process up and running
func Supervise(d *Daemon) {
	var (
		err  error
		p    *process
		pid  int
		wait time.Duration
	)

	// start a new process
	p, err = d.Run(NewProcess(d.cfg))
	if err != nil {
		log.Fatal(err)
	}

	for {
		select {
		case <-d.quit:
			return
		case <-d.run:
			time.Sleep(wait)
			// create a new process
			if d.lock == 0 {
				np := NewProcess(d.cfg)
				if p, err = d.Run(np); err != nil {
					close(np.quit)
					log.Print(err)
					wait = time.Second
					d.run <- struct{}{}
				}
			}
		case err := <-p.errch:
			// set end time
			p.eTime = time.Now()
			// unlock, or lock once
			atomic.StoreUint32(&d.lock, d.lockOnce)
			if err != nil && err.Error() == "EXIT" {
				log.Printf("PID: %d (%s) Exited", pid, p.cmd.Path)
			} else {
				log.Printf("PID %d (%s) terminated, %s [%v user  %v sys  %s up]\n",
					p.cmd.ProcessState.Pid(),
					p.cmd.Path,
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
			// follow the new pid instead of trying to call run again unless the new pid dies
			if d.cfg.Pid.Follow != "" {
				pid, err = d.ReadPidFile(d.cfg.Pid.Follow)
				if err != nil {
					log.Printf("Cannot read pidfile: %s, %s", d.cfg.Pid.Follow, err)
					d.run <- struct{}{}
				} else {
					// check if pid in file is valid
					if pid > 1 && pid != p.Pid() && d.IsRunning(pid) {
						log.Printf("Watching pid %d on file: %s", pid, d.cfg.Pid.Follow)
						d.fpid = true
						// overwrite original (defunct) pid with the fpid in order to be available to send signals
						p.cmd.Process.Pid = pid
						d.WatchPid(pid, p.errch)
					} else {
						// if cmd exits or process is kill
						d.run <- struct{}{}
					}
				}
			} else {
				// run again
				d.run <- struct{}{}
			}
		}
	}
}
