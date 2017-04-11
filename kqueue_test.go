// +build freebsd netbsd openbsd dragonfly darwin

package immortal

import (
	"io/ioutil"
	"log"
	"os"
	"path/filepath"
	"testing"
	"time"
)

func TestWatchDir(t *testing.T) {
	ch := make(chan struct{})
	dir, err := ioutil.TempDir("", "TestWatchDir")
	if err != nil {
		t.Error(err)
	}

	go WatchDir(dir, ch)

	defer os.RemoveAll(dir) // clean up

	time.Sleep(100 * time.Millisecond)

	tmpfn := filepath.Join(dir, "tmpfile")
	if err = ioutil.WriteFile(tmpfn, []byte("something"), 0640); err != nil {
		t.Error(err)
	}

	select {
	case <-ch:
		return
	case <-time.After(time.Second):
		t.Error("Expecting struct on channel")
	}
}

func TestWatchDirBadDir(t *testing.T) {
	ch := make(chan struct{})
	err := WatchDir("/dev/null/none/existent", ch)
	if err == nil {
		t.Error("Expecting error")
	}
}

func TestWatchFile(t *testing.T) {
	ch := make(chan string, 1)
	tmpfile, err := ioutil.TempFile("", "TestWatchFile")
	if err != nil {
		log.Fatal(err)
	}
	if _, err := tmpfile.Write([]byte("something")); err != nil {
		t.Error(err)
	}
	if err := tmpfile.Close(); err != nil {
		t.Error(err)
	}
	defer os.Remove(tmpfile.Name()) // clean up

	go func() {
		err := WatchFile(tmpfile.Name(), ch)
		if err != nil {
			t.Error(err)
		}
	}()

	time.Sleep(100 * time.Millisecond)

	err = ioutil.WriteFile(tmpfile.Name(), []byte("foo"), 0644)
	if err != nil {
		t.Error(err)
	}

	select {
	case file := <-ch:
		expect(t, file, tmpfile.Name())
	case <-time.After(time.Second):
		t.Error("timeout waiting for channel")
	}
}

func TestWatchFileBadFile(t *testing.T) {
	ch := make(chan string, 1)
	err := WatchFile("/dev/null/none-existen", ch)
	if err == nil {
		t.Error("Expecting error")
	}
}
