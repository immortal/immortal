package immortal

import (
	//	"io"
	"os"
	"testing"
	//	"time"
)

func TestRunHelperProcess(*testing.T) {
	if os.Getenv("GO_WANT_HELPER_PROCESS") != "1" {
		return
	}
}

func TestRun(t *testing.T) {
	p := ""
	ctrl := false
	//	_, w := io.Pipe()
	w := ""
	_, e := New(nil, &p, &p, &p, &p, &w, &p, &p, &p, []string{"go", "test", "-run", "TestRunHelperProcess"}, &ctrl)
	if e != nil {
		t.Error(e)
	}
}
