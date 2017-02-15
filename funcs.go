package immortal

import (
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

// DurationRound
// https://play.golang.org/p/QHocTHl8iR
func DurationRound(d, r time.Duration) time.Duration {
	if r <= 0 {
		return d
	}
	neg := d < 0
	if neg {
		d = -d
	}
	if m := d % r; m+m < r {
		d = d - m
	} else {
		d = d + r - m
	}
	if neg {
		return -d
	}
	return d
}
