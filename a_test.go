package immortal

import (
	"encoding/json"
	"fmt"
	"net"
	"net/http"
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

// getJSON unix socket web client
func getJSON(path string, target interface{}) error {
	// http socket client
	tr := &http.Transport{
		Dial: func(proto, addr string) (net.Conn, error) {
			return net.Dial("unix", "supervise/immortal.sock")
		},
	}
	client := &http.Client{Transport: tr}
	r, err := client.Get(fmt.Sprintf("http://immortal.sock/%s", path))
	if err != nil {
		return err
	}
	defer r.Body.Close()
	return json.NewDecoder(r.Body).Decode(target)
}
