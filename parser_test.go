package immortal

import (
	"bytes"
	"errors"
	"flag"
	"os"
	"reflect"
	"testing"
)

/* Test Helpers */
func expect(t *testing.T, a interface{}, b interface{}) {
	if a != b {
		t.Errorf("Expected: %v (type %v)  Got: %v (type %v)", a, reflect.TypeOf(a), b, reflect.TypeOf(b))
	}
}

func expectDeepEqual(t *testing.T, a interface{}, b interface{}) {
	if !reflect.DeepEqual(a, b) {
		t.Errorf("Expected: %v (type %v)  Got: %v (type %v)", a, reflect.TypeOf(a), b, reflect.TypeOf(b))
	}
}

func TestParseExist(t *testing.T) {
	p := Parse{}
	expect(t, false, p.exists("/dev/null/non-existent"))
	expect(t, true, p.exists("/"))
}

func TestParseHelp(t *testing.T) {
	oldArgs := os.Args
	defer func() { os.Args = oldArgs }()
	os.Args = []string{"cmd", "-h"}
	p := &Parse{}
	fs := flag.NewFlagSet("test", flag.ContinueOnError)

	// Error output buffer
	buf := bytes.NewBuffer([]byte{})
	fs.SetOutput(buf)

	_, err := p.Parse(fs)
	if err == nil {
		expect(t, errors.New(""), err)
	}
}

func TestParseDefault(t *testing.T) {
	oldArgs := os.Args
	defer func() { os.Args = oldArgs }()
	os.Args = []string{"cmd", ""}
	p := &Parse{}
	fs := flag.NewFlagSet("test", flag.ContinueOnError)

	// Error output buffer
	buf := bytes.NewBuffer([]byte{})
	fs.SetOutput(buf)

	flags, err := p.Parse(fs)
	if err != nil {
		t.Error(err)
	}
	expect(t, false, flags.Ctrl)
	expect(t, false, flags.Version)
	expect(t, "", flags.Configfile)
	expect(t, "", flags.Wrkdir)
	expect(t, "", flags.Envdir)
	expect(t, "", flags.FollowPid)
	expect(t, "", flags.Logfile)
	expect(t, "", flags.Logger)
	expect(t, "", flags.ChildPid)
	expect(t, "", flags.ParentPid)
	expect(t, "", flags.User)
}

func TestParseFlags(t *testing.T) {
	oldArgs := os.Args
	defer func() { os.Args = oldArgs }()
	var flagTest = []struct {
		flag     []string
		name     string
		expected interface{}
	}{
		{[]string{"cmd", "-v"}, "Version", true},
		{[]string{"cmd", "-ctrl"}, "Ctrl", true},
		{[]string{"cmd", "-c", "run.yml"}, "Configfile", "run.yml"},
		{[]string{"cmd", "-d", "/arena/wrkdir"}, "Wrkdir", "/arena/wrkdir"},
		{[]string{"cmd", "-e", "/path/to/envdir"}, "Envdir", "/path/to/envdir"},
		{[]string{"cmd", "-f", "/path/to/pid"}, "FollowPid", "/path/to/pid"},
		{[]string{"cmd", "-l", "/path/to/log"}, "Logfile", "/path/to/log"},
		{[]string{"cmd", "-logger", "logger"}, "Logger", "logger"},
		{[]string{"cmd", "-p", "/path/to/child"}, "ChildPid", "/path/to/child"},
		{[]string{"cmd", "-P", "/path/to/parent"}, "ParentPid", "/path/to/parent"},
		{[]string{"cmd", "-u", "nbari"}, "User", "nbari"},
	}
	for _, f := range flagTest {
		os.Args = []string{}
		os.Args = f.flag
		p := &Parse{}
		fs := flag.NewFlagSet("test", flag.ContinueOnError)

		// Error output buffer
		buf := bytes.NewBuffer([]byte{})
		fs.SetOutput(buf)

		flags, err := p.Parse(fs)
		if err != nil {
			t.Error(err)
		}
		refValue := reflect.ValueOf(flags).Elem().FieldByName(f.name)
		switch refValue.Kind() {
		case reflect.Bool:
			expect(t, f.expected, refValue.Bool())
		case reflect.String:
			expect(t, f.expected, refValue.String())
		}
	}
}
