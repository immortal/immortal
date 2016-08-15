package immortal

import (
	"os"
	"syscall"
	"testing"
)

func TestFork(t *testing.T) {
	var pid int
	defer func() {
		syscall.Kill(pid, syscall.SIGKILL)
	}()
	f := Fork{}
	pid, err := f.Fork()
	if err != nil {
		t.Error(err)
	}
	if pid == os.Getpid() {
		t.Error("Expecting different pid")
	}
}

func TestForkErr(t *testing.T) {
	oldArgs := os.Args
	defer func() { os.Args = oldArgs }()
	os.Args = []string{""}
	f := Fork{}
	_, err := f.Fork()
	if err == nil {
		t.Error("Expecting error: fork/exec : no such file or directory")
	}
}
