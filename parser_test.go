package immortal

import (
	"bytes"
	"errors"
	"flag"
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
	p := &Parse{
		args: []string{"-h"},
	}

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
	p := &Parse{
		args: []string{""},
	}

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
	var flagTest = []struct {
		flag     []string
		name     string
		expected interface{}
	}{
		{[]string{"-v"}, "Version", true},
		{[]string{"-ctrl"}, "Ctrl", true},
		{[]string{"-c", "run.yml"}, "Configfile", "run.yml"},
		{[]string{"-d", "/arena/wrkdir"}, "Wrkdir", "/arena/wrkdir"},
		{[]string{"-e", "/path/to/envdir"}, "Envdir", "/path/to/envdir"},
	}
	for _, f := range flagTest {
		p := &Parse{
			args: f.flag,
		}
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
