package immortal

import (
	"bufio"
	"fmt"
	"os"
	"strconv"
	"syscall"
)

func (self *Daemon) Run(args []string) error {
	procAttr := new(os.ProcAttr)

	//	https://golang.org/pkg/syscall/#SysProcAttr
	if self.owner != nil {
		uid, err := strconv.Atoi(self.owner.Uid)
		if err != nil {
			return err
		}

		gid, err := strconv.Atoi(self.owner.Gid)
		if err != nil {
			return err
		}

		procAttr.Sys = &syscall.SysProcAttr{
			Credential: &syscall.Credential{
				Uid: uint32(uid),
				Gid: uint32(gid),
			},
		}
	}

	r, w, err := os.Pipe()
	if err != nil {
		panic(err)
	}
	defer r.Close()
	procAttr.Files = []*os.File{nil, w, w}
	process, err := os.StartProcess(args[0], args, procAttr)
	if err != nil {
		Log(fmt.Sprintf("ERROR Unable to run %s: %s\n", os.Args[0], err.Error()))
	}

	Log(fmt.Sprintf("%s running as pid %d", args[0], process.Pid))

	// write log
	in := bufio.NewScanner(r)
	for in.Scan() {
		Log(in.Text())
	}

	processState, err := process.Wait()
	if err != nil {
		Log(err.Error())
	}
	Log(fmt.Sprintf("%#v", processState))

	return nil
}
