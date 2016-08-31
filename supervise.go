package immortal

import (
	"log"
	"os"
	"os/signal"
	"sync/atomic"
	"syscall"
	"time"
)

func Supervise(d *Daemon) {
	var (
		err  error
		info = make(chan os.Signal)
		p    *process
		pid  int
		run  = make(chan struct{}, 1)
	)

	// start a new process
	p, err = d.Run(NewProcess(d.cfg))
	if err != nil {
		log.Fatal(err)
	}

	// Info loop kill 3 pid get stats
	signal.Notify(info, syscall.SIGQUIT)
	go d.Info(info)

	// create a supervisor
	s := &Sup{p}

	// listen on control for signals
	if d.cfg.ctrl {
		s.ReadFifoControl(d.fifo_control, d.fifo)
	}

	for {
		select {
		case <-d.quit:
			return
		case <-run:
			// create a new process
			p, err = d.Run(NewProcess(d.cfg))
			if err != nil {
				log.Print(err)
			}
			s = &Sup{p}
		default:
			select {
			case err := <-p.errch:
				// unlock, or lock once
				atomic.StoreUint32(&d.lock, d.lock_once)
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
				}

				// follow the new pid and stop running the command
				// unless the new pid dies
				if d.cfg.Pid.Follow != "" {
					pid, err = s.ReadPidFile(d.cfg.Pid.Follow)
					if err != nil {
						log.Printf("Cannot read pidfile:%s, %s", d.cfg.Pid.Follow, err)
						run <- struct{}{}
					} else {
						// check if pid in file is valid
						if pid > 1 && pid != p.Pid() && s.IsRunning(pid) {
							log.Printf("Watching pid %d on file: %s", pid, d.cfg.Pid.Follow)
							s.WatchPid(pid, p.errch)
						} else {
							// if cmd exits or process is kill
							run <- struct{}{}
						}
					}
				} else {
					run <- struct{}{}
				}
			case fifo := <-d.fifo:
				if fifo.err != nil {
					log.Printf("control error: %s", fifo.err)
				}
				s.HandleSignals(fifo.msg, d)
			}
		}
	}
}
