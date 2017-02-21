package immortal

import (
	"bytes"
	"encoding/json"
	"fmt"
	"math"
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

// TimeDiff return format days, hours, minutes, seconds
func TimeDiff(t time.Time) string {
	diff := time.Since(t)
	days := diff / (24 * time.Hour)
	hours := diff % (24 * time.Hour)
	minutes := hours % time.Hour
	seconds := math.Mod(minutes.Seconds(), 60)
	var buffer bytes.Buffer
	if days > 0 {
		buffer.WriteString(fmt.Sprintf("%dd", days))
	}
	if hours/time.Hour > 0 {
		buffer.WriteString(fmt.Sprintf("%dh", hours/time.Hour))
	}
	if minutes/time.Minute > 0 {
		buffer.WriteString(fmt.Sprintf("%dm", minutes/time.Minute))
	}
	if seconds > 0 {
		buffer.WriteString(fmt.Sprintf("%.1fs", seconds))
	}
	return buffer.String()
}
