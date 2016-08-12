package immortal

import (
	"reflect"
	"runtime"
	"testing"
)

/* Test Helpers */
func expect(t *testing.T, a interface{}, b interface{}) {
	_, fn, line, _ := runtime.Caller(1)
	if a != b {
		t.Fatalf("Expected: %v (type %v)  Got: %v (type %v)  in %s:%d", a, reflect.TypeOf(a), b, reflect.TypeOf(b), fn, line)
	}
}

type myFork struct{}

func (self myFork) Fork() (int, error) {
	return 0, nil
}
