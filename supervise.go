package immortal

import (
	"log"
	"os"
	"os/exec"
	"os/signal"
	"sync/atomic"
	"syscall"
	"time"
)

func Supervise(d *Daemon) {
	// start a new process
	p, err := d.Run(NewProcess(d.cfg))
	if err != nil {
		log.Fatal(err)
	}

	// Info loop kill 3 pid get stats
	info := make(chan os.Signal)
	signal.Notify(info, syscall.SIGQUIT)
	go d.Info(info)

	// create a supervisor
	s := &Sup{p}

	// listen on control for signals
	if d.cfg.ctrl {
		s.ReadFifoControl(d.fifo_control, d.fifo)
	}

	//run
	run := make(chan struct{}, 1)

	for {
		select {
		case <-d.quit:
			return
		case <-run:
			p, err := d.Run(NewProcess(d.cfg))
			if err != nil {
				log.Print(err)
			}
			s = &Sup{p}
		default:
			select {
			case err := <-p.errch:
				if err != nil {
					if exitError, ok := err.(*exec.ExitError); ok {
						atomic.StoreUint32(&d.lock, d.lock_once)
						log.Printf("PID %d terminated, %s [%v user  %v sys  %s up]\n",
							exitError.Pid(),
							exitError,
							exitError.UserTime(),
							exitError.SystemTime(),
							time.Since(p.sTime),
						)
					} else if err.Error() == "EXIT" {
						log.Printf("PID: %d Exited", p.Pid())
					} else {
						log.Print(err)
					}
				}

				// follow the new pid and stop running the command
				// unless the new pid dies
				if d.cfg.Pid.Follow != "" {
					pid, err := s.ReadPidFile(d.cfg.Pid.Follow)
					if err != nil {
						log.Printf("Cannot read pidfile:%s,  %s", d.cfg.Pid.Follow, err)
						run <- struct{}{}
					} else {
						// check if pid in file is valid
						if pid > 1 && pid != p.Pid() && s.IsRunning(pid) {
							log.Printf("Watching pid %d on file: %s", pid, d.cfg.Pid.Follow)
							go s.WatchPid(pid, d.done)
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
