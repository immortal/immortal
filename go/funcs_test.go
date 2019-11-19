package immortal

import (
	"io/ioutil"
	"os"
	"path/filepath"
	"testing"
	"time"
)

func TestAbsSince(t *testing.T) {
	start := time.Unix(0, 0)
	diff := AbsSince(start)
	if len(diff) < 12 {
		t.Errorf("Check that systems clock are in sync, diff: %s", diff)
	}
}

func TestMd5sumNonexistent(t *testing.T) {
	_, err := md5sum("/dev/null/non-existent")
	if err == nil {
		t.Errorf("Expecting an error")
	}
}

func TestMd5sum(t *testing.T) {
	tmpfile, err := ioutil.TempFile("", "md5")
	if err != nil {
		t.Error(err)
	}
	defer os.Remove(tmpfile.Name())
	content := []byte("The quick brown fox jumps over the lazy dog")
	if _, err := tmpfile.Write(content); err != nil {
		t.Error(err)
	}
	if err := tmpfile.Close(); err != nil {
		t.Error(err)
	}
	md5, err := md5sum(tmpfile.Name())
	if err != nil {
		t.Error(err)
	}
	expect(t, "9e107d9d372bb6826bd81d3542a419d6", md5)
}

func TestInSlice(t *testing.T) {
	var test = []struct {
		slice  []string
		item   string
		expect bool
	}{
		{[]string{"a", "b", "c"}, "x", false},
		{[]string{"a", "b", "c"}, "a", true},
		{[]string{"a a", "b b", "c c"}, "b b", true},
		{[]string{" a", " b", " c"}, "a", false},
		{[]string{" a ", " b ", " c "}, " a ", true},
	}
	for _, tt := range test {
		if i := inSlice(tt.slice, tt.item); i != tt.expect {
			t.Error(tt.slice)
		}
	}
}

func TestGetUserSdir(t *testing.T) {
	oldHome := os.Getenv("HOME")

	os.Setenv("HOME", "/tmp/foo")
	if uSdir, err := GetUserSdir(); err != nil {
		t.Error(err)
	} else {
		expect(t, uSdir, filepath.Join("/tmp/foo", ".immortal"))
	}

	os.Setenv("HOME", "")
	if uSdir, err := GetUserSdir(); err != nil {
		t.Error(err)
	} else {
		expect(t, uSdir, filepath.Join(oldHome, ".immortal"))
	}

	os.Setenv("HOME", oldHome)
}

func TestIsDir(t *testing.T) {
	expect(t, false, isDir("/dev/null"))
	expect(t, true, isDir("/"))
	expect(t, false, isDir("/etc/hosts"))
}

func TestIsFile(t *testing.T) {
	expect(t, false, isFile("/dev/null"))
	expect(t, false, isFile("/"))
	expect(t, true, isFile("/etc/hosts"))
}
