package immortal

import (
	"io/ioutil"
	"os"
	"testing"
)

func TestSocketListenError(t *testing.T) {
	sdir, err := ioutil.TempDir("", "TestSocketListenError")
	if err != nil {
		t.Error(err)
	}
	cfg := &Config{
		Env:     map[string]string{"GO_WANT_HELPER_PROCESS": "signalsUDOT"},
		command: []string{os.Args[0]},
		ctl:     sdir,
	}
	// create new daemon
	d, err := New(cfg)
	if err != nil {
		t.Fatal(err)
	}
	os.RemoveAll(sdir)

	// create socket should return error
	if err := d.Listen(); err == nil {
		t.Fatal(err)
	}
}
