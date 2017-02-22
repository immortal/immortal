package immortal

import (
	"bytes"
	"encoding/json"
	"fmt"
	"net"
	"net/http"
	"strconv"
	"time"
)

const (
	logo = "2B55"
)

// Logo print ⭕
func Logo() rune {
	return Icon(logo)
}

// Icon Unicode Hex to string
func Icon(h string) rune {
	i, e := strconv.ParseInt(h, 16, 32)
	if e != nil {
		return 0
	}
	return rune(i)
}

// getJSON unix socket web client
func GetJSON(spath, path string, target interface{}) error {
	// http socket client
	tr := &http.Transport{
		Dial: func(proto, addr string) (net.Conn, error) {
			return net.Dial("unix", spath)
		},
	}

	client := &http.Client{Transport: tr}
	r, err := client.Get(fmt.Sprintf("http://socket/%s", path))
	if err != nil {
		return err
	}

	defer r.Body.Close()
	return json.NewDecoder(r.Body).Decode(target)
}

// AbsSince return format days, hours, minutes, seconds
func AbsSince(t time.Time) string {
	const (
		Decisecond = 100 * time.Millisecond
		Day        = 24 * time.Hour
	)
	ts := time.Since(t) + Decisecond/2
	d := ts / Day
	ts = ts % Day
	h := ts / time.Hour
	ts = ts % time.Hour
	m := ts / time.Minute
	ts = ts % time.Minute
	s := ts / time.Second
	ts = ts % time.Second
	f := ts / Decisecond
	var buffer bytes.Buffer
	if d > 0 {
		buffer.WriteString(fmt.Sprintf("%dd", d))
	}
	if h > 0 {
		buffer.WriteString(fmt.Sprintf("%dh", h))
	}
	if m > 0 {
		buffer.WriteString(fmt.Sprintf("%dm", m))
	}
	buffer.WriteString(fmt.Sprintf("%d.%ds", s, f))
	return buffer.String()
}
