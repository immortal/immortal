package immortal

import (
	"bytes"
	"flag"
	"fmt"
	"io/ioutil"
	"os"
	"os/user"
	"reflect"
	"testing"
)

func MockLookup(username string) (*user.User, error) {
	switch {
	case username == "www":
		return new(user.User), nil
	case username == "nonexistent":
		return nil, user.UnknownUserError("nonexistent")
	}
	return nil, fmt.Errorf("error")
}

func TestParseHelp(t *testing.T) {
	oldArgs := os.Args
	defer func() { os.Args = oldArgs }()
	os.Args = []string{"cmd", "-h"}
	p := &Parse{}
	fs := flag.NewFlagSet("test", flag.ContinueOnError)
	fs.Usage = p.Usage(fs)
	// Error output buffer
	buf := bytes.NewBuffer([]byte{})
	fs.SetOutput(buf)
	_, w, err := os.Pipe()
	if err != nil {
		t.Error(err)
	}
	os.Stderr = w
	_, err = p.Parse(fs)
	if err == nil {
		t.Error("Expecting error")
	}
}

func TestParseDefault(t *testing.T) {
	oldArgs := os.Args
	defer func() { os.Args = oldArgs }()
	os.Args = []string{"cmd", ""}
	p := &Parse{}
	var helpCalled = false
	fs := flag.NewFlagSet("test", flag.ContinueOnError)
	fs.Usage = func() { helpCalled = true }
	flags, err := p.Parse(fs)
	if err != nil {
		t.Error(err)
	}
	if helpCalled {
		t.Error("help called for regular flag")
	}
	expect(t, "", flags.ChildPid)
	expect(t, "", flags.Configfile)
	expect(t, "", flags.Ctl)
	expect(t, "", flags.Envdir)
	expect(t, "", flags.FollowPid)
	expect(t, "", flags.Logfile)
	expect(t, "", flags.Logger)
	expect(t, "", flags.ParentPid)
	expect(t, "", flags.User)
	expect(t, "", flags.Wrkdir)
	expect(t, -1, flags.Retries)
	expect(t, false, flags.Nodaemon)
	expect(t, false, flags.Version)
	expect(t, uint(0), flags.Wait)
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
		{[]string{"cmd", "-n"}, "Nodaemon", true},
		{[]string{"cmd", "-ctl", "service"}, "Ctl", "service"},
		{[]string{"cmd", "-c", "run.yml"}, "Configfile", "run.yml"},
		{[]string{"cmd", "-d", "/arena/wrkdir"}, "Wrkdir", "/arena/wrkdir"},
		{[]string{"cmd", "-e", "/path/to/envdir"}, "Envdir", "/path/to/envdir"},
		{[]string{"cmd", "-f", "/path/to/pid"}, "FollowPid", "/path/to/pid"},
		{[]string{"cmd", "-l", "/path/to/log"}, "Logfile", "/path/to/log"},
		{[]string{"cmd", "-logger", "logger"}, "Logger", "logger"},
		{[]string{"cmd", "-p", "/path/to/child"}, "ChildPid", "/path/to/child"},
		{[]string{"cmd", "-P", "/path/to/parent"}, "ParentPid", "/path/to/parent"},
		{[]string{"cmd", "-u", "nbari"}, "User", "nbari"},
		{[]string{"cmd", "-w", "3"}, "Wait", 3},
		{[]string{"cmd", "-r", "2"}, "Retries", 2},
		{[]string{"cmd"}, "Nodaemon", false},
		{[]string{"cmd"}, "Retries", -1},
		{[]string{"cmd"}, "Wait", 0},
	}

	var helpCalled = false
	for _, f := range flagTest {
		os.Args = f.flag
		p := &Parse{}
		fs := flag.NewFlagSet("test", flag.ContinueOnError)
		fs.Usage = func() { helpCalled = true }
		flags, err := p.Parse(fs)
		if err != nil {
			t.Error(err)
		}
		if helpCalled {
			t.Error("help called for regular flag")
			helpCalled = false // reset for next test
		}
		refValue := reflect.ValueOf(flags).Elem().FieldByName(f.name)
		switch refValue.Kind() {
		case reflect.Bool:
			expect(t, f.expected, refValue.Bool())
		case reflect.String:
			expect(t, f.expected, refValue.String())
		case reflect.Int:
			expect(t, f.expected, int(refValue.Int()))
		case reflect.Uint:
			expect(t, uint(f.expected.(int)), uint(refValue.Uint()))
		}
	}
}

func TestParseArgsHelp(t *testing.T) {
	oldArgs := os.Args
	defer func() { os.Args = oldArgs }()
	os.Args = []string{"cmd", "-h"}
	parser := &Parse{}
	var helpCalled = false
	fs := flag.NewFlagSet("TestParseArgsHelp", flag.ContinueOnError)
	fs.Usage = func() { helpCalled = true }
	ParseArgs(parser, fs)
	if !helpCalled {
		t.Fatal("help was not called")
	}
}

func TestParseArgsVersion(t *testing.T) {
	oldArgs := os.Args
	defer func() { os.Args = oldArgs }()
	os.Args = []string{"cmd", "-v"}
	parser := &Parse{}
	var helpCalled = false
	fs := flag.NewFlagSet("TestParseArgsVersion", flag.ContinueOnError)
	fs.Usage = func() { helpCalled = true }
	ParseArgs(parser, fs)
	if helpCalled {
		t.Error("help called for regular flag")
	}
}

func TestParseArgsVersion2(t *testing.T) {
	oldArgs := os.Args
	defer func() { os.Args = oldArgs }()
	os.Args = []string{"cmd", "-v", "-c", "xyz"}
	parser := &Parse{}
	var helpCalled = false
	fs := flag.NewFlagSet("TestParseArgsVersion2", flag.ContinueOnError)
	fs.Usage = func() { helpCalled = true }
	ParseArgs(parser, fs)
	if helpCalled {
		t.Error("help called for regular flag")
	}
}

func TestParseArgsCtl(t *testing.T) {
	oldArgs := os.Args
	defer func() { os.Args = oldArgs }()
	var flagTest = []struct {
		flag     []string
		expected string
	}{
		{[]string{"cmd", "-ctl", "/service", "xyz"}, "/service"},
		{[]string{"cmd", "-ctl", "service", "xyz"}, "/var/run/immortal/service"},
		{[]string{"cmd", "-ctl", "123", "xyz"}, "/var/run/immortal/123"},
		{[]string{"cmd", "-ctl", "123/456", "xyz"}, "/var/run/immortal/456"},
		{[]string{"cmd", "-ctl", "123/../456", "xyz"}, "/var/run/immortal/456"},
		{[]string{"cmd", "-ctl", "../123/../456", "xyz"}, "/var/run/immortal/456"},
		{[]string{"cmd", "-ctl", "../foo", "xyz"}, "/var/run/immortal/foo"},
		{[]string{"cmd", "-ctl", "", "xyz"}, ""},
		{[]string{"cmd", "-ctl", "~/user", "xyz"}, "/var/run/immortal/user"},
		{[]string{"cmd", "-ctl", "/tmp/test/", "xyz"}, "/tmp/test"},
	}
	for _, f := range flagTest {
		os.Args = f.flag
		parser := &Parse{}
		var helpCalled = false
		fs := flag.NewFlagSet("TestParseArgsCtl", flag.ContinueOnError)
		fs.Usage = func() { helpCalled = true }
		cfg, err := ParseArgs(parser, fs)
		if err != nil {
			t.Error(err)
		}
		if helpCalled {
			t.Error("help called for regular flag")
		}
		expect(t, cfg.ctl, f.expected)
	}
}

func TestParseArgsNoargs(t *testing.T) {
	oldArgs := os.Args
	defer func() { os.Args = oldArgs }()
	os.Args = []string{"cmd"}
	parser := &Parse{}
	var helpCalled = false
	fs := flag.NewFlagSet("TestParseArgsNoargs", flag.ContinueOnError)
	fs.Usage = func() { helpCalled = true }
	_, err := ParseArgs(parser, fs)
	if helpCalled {
		t.Error("help called for regular flag")
	}
	if err == nil {
		t.Error("Expecting error")
	}
}

func TestParseArgsTable(t *testing.T) {
	oldArgs := os.Args
	defer func() { os.Args = oldArgs }()
	dir, err := ioutil.TempDir("", "TestParseArgsTable")
	if err != nil {
		t.Fatal(err)
	}
	defer os.RemoveAll(dir)
	var flagTest = []struct {
		flag        []string
		expectError bool
	}{
		{[]string{"cmd", "-v"}, false},
		{[]string{"cmd", "-ctl", "service"}, true},
		{[]string{"cmd", "-ctl", "service", "cmd"}, false},
		{[]string{"cmd", "-c", "run.yml"}, true},
		{[]string{"cmd", "-c", "run.yml", "cmd"}, true},
		{[]string{"cmd", "-c", "example/run.yml", "cmd"}, false},
		{[]string{"cmd", "-d", "/arena/wrkdir"}, true},
		{[]string{"cmd", "-d", "/dev/null", "cmd"}, true},
		{[]string{"cmd", "-d", dir, "cmd"}, false},
		{[]string{"cmd", "-e", "/path/to/envdir"}, true},
		{[]string{"cmd", "-e", "/dev/null", "cmd"}, true},
		{[]string{"cmd", "-e", "example/env", "cmd"}, false},
		{[]string{"cmd", "-e", dir, "cmd"}, false},
		{[]string{"cmd", "-f", "/path/to/pid"}, true},
		{[]string{"cmd", "-f", "/path/to/pid", "cmd"}, false},
		{[]string{"cmd", "-l", "/path/to/log"}, true},
		{[]string{"cmd", "-l", "/path/to/log", "cmd"}, false},
		{[]string{"cmd", "-logger", "logger"}, true},
		{[]string{"cmd", "-logger", "logger", "cmd"}, false},
		{[]string{"cmd", "-p", "/path/to/child"}, true},
		{[]string{"cmd", "-p", "/path/to/child", "cmd"}, false},
		{[]string{"cmd", "-P", "/path/to/parent"}, true},
		{[]string{"cmd", "-P", "/path/to/parent", "cmd"}, false},
		{[]string{"cmd", "-w", "30"}, true},
		{[]string{"cmd", "-w", "30", "cmd"}, false},
		{[]string{"cmd", "-u", "www"}, true},
		{[]string{"cmd", "-u", "www", "cmd"}, false},
		{[]string{"cmd", "-u", "nonexistent", "cmd"}, true},
		{[]string{"cmd", "-u", "err!=nil", "cmd"}, true},
		{[]string{"cmd", "-r", "1"}, true},
		{[]string{"cmd", "-r", "1", "cmd"}, false},
	}
	var helpCalled = false
	for _, f := range flagTest {
		os.Args = f.flag
		parser := &Parse{
			UserLookup: MockLookup,
		}
		fs := flag.NewFlagSet("TestParseArgsTable", flag.ContinueOnError)
		fs.Usage = func() { helpCalled = true }
		_, err := ParseArgs(parser, fs)
		if f.expectError {
			if err == nil {
				t.Error("Expecting error")
			}
		} else {
			if err != nil {
				t.Error(err)
			}
		}
		if helpCalled {
			t.Error("help called for regular flag")
			helpCalled = false // reset for next test
		}
	}
}

func TestParseYamlCmd(t *testing.T) {
	tmpfile, err := ioutil.TempFile("", "TestParseYamlCmd")
	if err != nil {
		t.Error(err)
	}
	defer os.Remove(tmpfile.Name())
	yaml := []byte(`
cwd: /tmp/
env:
    DEBUG: 1
    ENVIRONMENT: production
pid:
    follow: /path/to/unicorn.pid
    parent: /tmp/parent.pid
    child: /tmp/child.pid
log:
    file: /var/log/app.log
    age: 86400 # seconds
    num: 7     # int
    size: 1    # MegaBytes
logger: logger -t unicorn
user: www
wait: 1`)
	err = ioutil.WriteFile(tmpfile.Name(), yaml, 0644)
	if err != nil {
		t.Error(err)
	}
	oldArgs := os.Args
	defer func() { os.Args = oldArgs }()
	os.Args = []string{"cmd", "-c", tmpfile.Name()}
	parser := &Parse{
		UserLookup: MockLookup,
	}
	var helpCalled = false
	fs := flag.NewFlagSet("TestParseArgsYaml", flag.ContinueOnError)
	fs.Usage = func() { helpCalled = true }
	_, err = ParseArgs(parser, fs)
	if helpCalled {
		t.Error("help called for regular flag")
	}
	if err == nil {
		t.Error("Expecting error: Missing command")
	}
}

func TestParseYamlCwd(t *testing.T) {
	tmpfile, err := ioutil.TempFile("", "TestParseYamlCwd")
	if err != nil {
		t.Error(err)
	}
	defer os.Remove(tmpfile.Name())
	yaml := []byte(`
cmd: command
cwd: /dev/null/nonexistent
env:
    DEBUG: 1
    ENVIRONMENT: production
pid:
    follow: /path/to/unicorn.pid
    parent: /tmp/parent.pid
    child: /tmp/child.pid
log:
    file: /var/log/app.log
    age: 86400 # seconds
    num: 7     # int
    size: 1    # MegaBytes
logger: logger -t unicorn
user: www
wait: 1`)
	err = ioutil.WriteFile(tmpfile.Name(), yaml, 0644)
	if err != nil {
		t.Error(err)
	}
	oldArgs := os.Args
	defer func() { os.Args = oldArgs }()
	os.Args = []string{"cmd", "-c", tmpfile.Name()}
	parser := &Parse{
		UserLookup: MockLookup,
	}
	var helpCalled = false
	fs := flag.NewFlagSet("TestParseArgsYamlCwd", flag.ContinueOnError)
	fs.Usage = func() { helpCalled = true }
	_, err = ParseArgs(parser, fs)
	if helpCalled {
		t.Error("help called for regular flag")
	}
	if err == nil {
		t.Error("Expecting error: Missing command")
	}
}

func TestParseYamlUsrErr(t *testing.T) {
	tmpfile, err := ioutil.TempFile("", "TestParseYamlUsrErr")
	if err != nil {
		t.Error(err)
	}
	defer os.Remove(tmpfile.Name())
	yaml := []byte(`
cmd: command
env:
    DEBUG: 1
    ENVIRONMENT: production
pid:
    follow: /path/to/unicorn.pid
    parent: /tmp/parent.pid
    child: /tmp/child.pid
log:
    file: /var/log/app.log
    age: 86400 # seconds
    num: 7     # int
    size: 1    # MegaBytes
logger: logger -t unicorn
user: nonexistent
wait: 1`)
	err = ioutil.WriteFile(tmpfile.Name(), yaml, 0644)
	if err != nil {
		t.Error(err)
	}
	oldArgs := os.Args
	defer func() { os.Args = oldArgs }()
	os.Args = []string{"cmd", "-c", tmpfile.Name()}
	parser := &Parse{
		UserLookup: MockLookup,
	}
	var helpCalled = false
	fs := flag.NewFlagSet("TestParseArgsYamlUsrErr", flag.ContinueOnError)
	fs.Usage = func() { helpCalled = true }
	_, err = ParseArgs(parser, fs)
	if helpCalled {
		t.Error("help called for regular flag")
	}
	if err == nil {
		t.Error("Expecting error")
	}
}

func TestParseYamlErr(t *testing.T) {
	tmpfile, err := ioutil.TempFile("", "TestParseYamlErr")
	if err != nil {
		t.Error(err)
	}
	defer os.Remove(tmpfile.Name())
	yaml := []byte(`
    num: 7     # int
    size: 1    # MegaBytes
logger: logger -t unicorn
user: nonexistent`)
	err = ioutil.WriteFile(tmpfile.Name(), yaml, 0644)
	if err != nil {
		t.Error(err)
	}
	oldArgs := os.Args
	defer func() { os.Args = oldArgs }()
	os.Args = []string{"cmd", "-c", tmpfile.Name()}
	parser := &Parse{
		UserLookup: MockLookup,
	}
	var helpCalled = false
	fs := flag.NewFlagSet("TestParseArgsYamlErr", flag.ContinueOnError)
	fs.Usage = func() { helpCalled = true }
	_, err = ParseArgs(parser, fs)
	if helpCalled {
		t.Error("help called for regular flag")
	}
	if err == nil {
		t.Error("Expecting error")
	}
}

func TestParseParseYmlioutil(t *testing.T) {
	p := &Parse{}
	if _, err := p.parseYml("/dev/null/non-existent"); err == nil {
		t.Error("Expecting error")
	}
}

func TestParseYamlRequire(t *testing.T) {
	tmpfile, err := ioutil.TempFile("", "TestParseYamlRequire")
	if err != nil {
		t.Error(err)
	}
	defer os.Remove(tmpfile.Name())
	yaml := []byte(`
cmd: command
wait: 1
require:
  - service1
  - service2`)
	err = ioutil.WriteFile(tmpfile.Name(), yaml, 0644)
	if err != nil {
		t.Error(err)
	}
	oldArgs := os.Args
	defer func() { os.Args = oldArgs }()
	os.Args = []string{"cmd", "-c", tmpfile.Name()}
	parser := &Parse{
		UserLookup: MockLookup,
	}
	var helpCalled = false
	fs := flag.NewFlagSet("TestParseArgsYamlUsrErr", flag.ContinueOnError)
	fs.Usage = func() { helpCalled = true }
	cfg, err := ParseArgs(parser, fs)
	if err != nil {
		t.Error(err)
	}
	if helpCalled {
		t.Error("help called for regular flag")
	}
	expect(t, len(cfg.Require), 2)
	expect(t, cfg.Require[0], "service1")
	expect(t, cfg.Require[1], "service2")
}

func TestParseYamlRequireEmpty(t *testing.T) {
	tmpfile, err := ioutil.TempFile("", "TestParseYamlRequire")
	if err != nil {
		t.Error(err)
	}
	defer os.Remove(tmpfile.Name())
	yaml := []byte(`
cmd: command
wait: 1`)
	err = ioutil.WriteFile(tmpfile.Name(), yaml, 0644)
	if err != nil {
		t.Error(err)
	}
	oldArgs := os.Args
	defer func() { os.Args = oldArgs }()
	os.Args = []string{"cmd", "-c", tmpfile.Name()}
	parser := &Parse{
		UserLookup: MockLookup,
	}
	var helpCalled = false
	fs := flag.NewFlagSet("TestParseArgsYamlUsrErr", flag.ContinueOnError)
	fs.Usage = func() { helpCalled = true }
	cfg, err := ParseArgs(parser, fs)
	if err != nil {
		t.Error(err)
	}
	if helpCalled {
		t.Error("help called for regular flag")
	}
	expect(t, len(cfg.Require), 0)
}

func TestParseYamlRequireCmd(t *testing.T) {
	tmpfile, err := ioutil.TempFile("", "TestParseYamlRequireCmd")
	if err != nil {
		t.Error(err)
	}
	defer os.Remove(tmpfile.Name())
	yaml := []byte(`
cmd: command
wait: 1
require_cmd: test -f /tmp/foo`)
	err = ioutil.WriteFile(tmpfile.Name(), yaml, 0644)
	if err != nil {
		t.Error(err)
	}
	oldArgs := os.Args
	defer func() { os.Args = oldArgs }()
	os.Args = []string{"cmd", "-c", tmpfile.Name()}
	parser := &Parse{
		UserLookup: MockLookup,
	}
	var helpCalled = false
	fs := flag.NewFlagSet("TestParseArgsYamlUsrErr", flag.ContinueOnError)
	fs.Usage = func() { helpCalled = true }
	cfg, err := ParseArgs(parser, fs)
	if err != nil {
		t.Error(err)
	}
	if helpCalled {
		t.Error("help called for regular flag")
	}
	expect(t, len(cfg.RequireCmd) > 0, true)
	expect(t, cfg.RequireCmd, "test -f /tmp/foo")
}

func TestParseYamlRequireEmptyCmd(t *testing.T) {
	tmpfile, err := ioutil.TempFile("", "TestParseYamlRequireCmd")
	if err != nil {
		t.Error(err)
	}
	defer os.Remove(tmpfile.Name())
	yaml := []byte(`
cmd: command
wait: 1`)
	err = ioutil.WriteFile(tmpfile.Name(), yaml, 0644)
	if err != nil {
		t.Error(err)
	}
	oldArgs := os.Args
	defer func() { os.Args = oldArgs }()
	os.Args = []string{"cmd", "-c", tmpfile.Name()}
	parser := &Parse{
		UserLookup: MockLookup,
	}
	var helpCalled = false
	fs := flag.NewFlagSet("TestParseArgsYamlUsrErr", flag.ContinueOnError)
	fs.Usage = func() { helpCalled = true }
	cfg, err := ParseArgs(parser, fs)
	if err != nil {
		t.Error(err)
	}
	if helpCalled {
		t.Error("help called for regular flag")
	}
	expect(t, len(cfg.RequireCmd), 0)
}

func TestParseYamlRetriesDefaults(t *testing.T) {
	tmpfile, err := ioutil.TempFile("", "TestParseYamlRetriesDefaults")
	if err != nil {
		t.Error(err)
	}
	defer os.Remove(tmpfile.Name())
	yaml := []byte(`
cmd: command`)
	err = ioutil.WriteFile(tmpfile.Name(), yaml, 0644)
	if err != nil {
		t.Error(err)
	}
	oldArgs := os.Args
	defer func() { os.Args = oldArgs }()
	os.Args = []string{"cmd", "-c", tmpfile.Name()}
	parser := &Parse{
		UserLookup: MockLookup,
	}
	var helpCalled = false
	fs := flag.NewFlagSet("TestParseArgsYamlUsrErr", flag.ContinueOnError)
	fs.Usage = func() { helpCalled = true }
	cfg, err := ParseArgs(parser, fs)
	if err != nil {
		t.Error(err)
	}
	if helpCalled {
		t.Error("help called for regular flag")
	}
	expect(t, -1, cfg.Retries)
}

func TestParseYamlCustomRetries0(t *testing.T) {
	tmpfile, err := ioutil.TempFile("", "TestParseYamlRetriesDefaults")
	if err != nil {
		t.Error(err)
	}
	defer os.Remove(tmpfile.Name())
	yaml := []byte(`
cmd: command
retries: 0`)
	err = ioutil.WriteFile(tmpfile.Name(), yaml, 0644)
	if err != nil {
		t.Error(err)
	}
	oldArgs := os.Args
	defer func() { os.Args = oldArgs }()
	os.Args = []string{"cmd", "-c", tmpfile.Name()}
	parser := &Parse{
		UserLookup: MockLookup,
	}
	var helpCalled = false
	fs := flag.NewFlagSet("TestParseArgsYamlUsrErr", flag.ContinueOnError)
	fs.Usage = func() { helpCalled = true }
	cfg, err := ParseArgs(parser, fs)
	if err != nil {
		t.Error(err)
	}
	if helpCalled {
		t.Error("help called for regular flag")
	}
	expect(t, 0, cfg.Retries)
}

func TestParseYamlCustomRetries10(t *testing.T) {
	tmpfile, err := ioutil.TempFile("", "TestParseYamlRetriesDefaults")
	if err != nil {
		t.Error(err)
	}
	defer os.Remove(tmpfile.Name())
	yaml := []byte(`
cmd: command
retries: 10`)
	err = ioutil.WriteFile(tmpfile.Name(), yaml, 0644)
	if err != nil {
		t.Error(err)
	}
	oldArgs := os.Args
	defer func() { os.Args = oldArgs }()
	os.Args = []string{"cmd", "-c", tmpfile.Name()}
	parser := &Parse{
		UserLookup: MockLookup,
	}
	var helpCalled = false
	fs := flag.NewFlagSet("TestParseArgsYamlUsrErr", flag.ContinueOnError)
	fs.Usage = func() { helpCalled = true }
	cfg, err := ParseArgs(parser, fs)
	if err != nil {
		t.Error(err)
	}
	if helpCalled {
		t.Error("help called for regular flag")
	}
	expect(t, 10, cfg.Retries)
}
