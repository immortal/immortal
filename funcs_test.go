package immortal

import (
	"io/ioutil"
	"os"
	"testing"
	"time"
)

func TestLogo(t *testing.T) {
	logo := Logo()
	if logo != 11093 {
		t.Errorf("Expecting: 11093 got: %v", logo)
	}
}

func TestIconOk(t *testing.T) {
	i := Icon("1F621")
	if i != 128545 {
		t.Errorf("Expecting: 128545 got: %v", i)
	}
}

func TestIconErr(t *testing.T) {
	i := Icon(" ")
	if i != 0 {
		t.Errorf("Expecting: 0 got: %v", i)
	}
}

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
