package immortal

import (
	"log"
	"sync/atomic"
	"time"
)

type Supervisor struct {
	daemon  *Daemon
	process *process
	wait    time.Duration
}

// Supervise keep daemon process up and running
func Supervise(d *Daemon) {
	// start a new process
	p, err := d.Run(NewProcess(d.cfg))
	if err != nil {
		log.Fatal(err)
	}
	supervisor := &Supervisor{
		daemon:  d,
		process: p,
	}
	supervisor.Start()
}

// ReStart create a new process
func (s *Supervisor) ReStart() {
	var err error
	np := NewProcess(s.daemon.cfg)
	if s.process, err = s.daemon.Run(np); err != nil {
		close(np.quit)
		log.Print(err)
		s.wait = time.Second
		s.daemon.run <- struct{}{}
	}
}

// Terminate
func (s *Supervisor) Terminate() {
	// set end time
	s.process.eTime = time.Now()
	// unlock, or lock once
	atomic.StoreUint32(&s.daemon.lock, s.daemon.lockOnce)
	if err != nil && err.Error() == "EXIT" {
		log.Printf("PID: %d (%s) Exited", s.pid, s.process.cmd.Path)
	} else {
		log.Printf("PID %d (%s) terminated, %s [%v user  %v sys  %s up]\n",
			s.process.cmd.ProcessState.Pid(),
			s.process.cmd.Path,
			s.process.cmd.ProcessState,
			s.process.cmd.ProcessState.UserTime(),
			s.process.cmd.ProcessState.SystemTime(),
			time.Since(s.process.sTime),
		)
		// calculate time for next reboot (avoids high CPU usage)
		uptime := s.process.eTime.Sub(s.process.sTime)
		s.wait = 0 * time.Second
		if uptime < time.Second {
			s.wait = time.Second - uptime
		}
	}
}

func (s *Supervisor) FollowPid() {
	var err error
	pid, err = s.daemon.ReadPidFile(s.daemon.cfg.Pid.Follow)
	if err != nil {
		log.Printf("Cannot read pidfile: %s, %s", s.daemon.cfg.Pid.Follow, err)
		s.daemon.run <- struct{}{}
	} else {
		// check if pid in file is valid
		if pid > 1 && pid != s.process.Pid() && s.daemon.IsRunning(pid) {
			log.Printf("Watching pid %d on file: %s", pid, d.cfg.Pid.Follow)
			s.daemon.fpid = true
			// overwrite original (defunct) pid with the fpid in order to be available to send signals
			s.process.cmd.Process.Pid = pid
			d.WatchPid(pid, s.process.errch)
		} else {
			// if cmd exits or process is kill
			s.daemon.run <- struct{}{}
		}
	}
}

func (s *Supervisor) Start() {
	for {
		select {
		case <-s.daemon.quit:
			return
		case <-s.daemon.run:
			time.Sleep(s.wait)
			// create a new process
			if s.daemon.lock == 0 {
				s.Restart()
			}
		case err := <-s.process.errch:
			// follow the new pid instead of trying to call run again unless the new pid dies
			if d.cfg.Pid.Follow != "" {
				s.FollowPid()
			} else {
				// run again
				s.daemon.run <- struct{}{}
			}
		}
	}
}
