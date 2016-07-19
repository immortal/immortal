package immortal

import (
	"io/ioutil"
	"strconv"
	"strings"
)

// readPidfile read pid from file if error returns pid 0
func (self *Daemon) readPidfile() (int, error) {
	content, err := ioutil.ReadFile(self.run.FollowPid)
	if err != nil {
		return 0, err
	}
	lines := strings.Split(string(content), "\n")
	pid, err := strconv.Atoi(lines[0])
	if err != nil {
		return 0, err
	}
	return pid, nil
}
