package immortal

import (
	"reflect"
	"testing"
)

/* Test Helpers */
func expect(t *testing.T, a interface{}, b interface{}) {
	if a != b {
		t.Errorf("Expected %v (type %v) - Got %v (type %v)", b, reflect.TypeOf(b), a, reflect.TypeOf(a))
	}
}

func expectDeepEqual(t *testing.T, a interface{}, b interface{}) {
	if !reflect.DeepEqual(a, b) {
		t.Errorf("Expected %v (type %v) - Got %v (type %v)", b, reflect.TypeOf(b), a, reflect.TypeOf(a))
	}
}

func TestParseExist(t *testing.T) {
	p := Parse{}
	expect(t, false, p.exists("/dev/null/non-existent"))
	expect(t, true, p.exists("/"))
}

func TestParse(t *testing.T) {
	p := &Parse{
		args: []string{"-h"},
	}
	flags := p.Parse()
	t.Logf("%#v", flags)
}
