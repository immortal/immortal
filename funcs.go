package immortal

import (
	"bytes"
	"crypto/md5"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"os/user"
	"path/filepath"
	"time"
)

// GetJSON unix socket web client
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

// AbsSince return time since in format [days]d[hours]h[minutes]m[seconds.decisecond]s
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

// md5sum return md5 checksum of given file
func md5sum(filePath string) (string, error) {
	f, err := os.Open(filePath)
	if err != nil {
		return "", err
	}
	defer f.Close()
	h := md5.New()
	if _, err := io.Copy(h, f); err != nil {
		return "", err
	}
	return fmt.Sprintf("%x", h.Sum(nil)), nil
}

// inSlice find if item is in slice
func inSlice(s []string, item string) bool {
	for _, i := range s {
		if i == item {
			return true
		}
	}
	return false
}

// GetUserdir returns the $HOME/.immortal
func GetUserdir() string {
	home := os.Getenv("HOME")
	if home == "" {
		usr, err := user.Current()
		if err == nil {
			home = usr.HomeDir
		}
	}
	return filepath.Join(home, ".immortal")
}

// GetSdir return the main supervise directory, defaults to /var/run/immortal
func GetSdir() string {
	// if IMMORTAL_SDIR env is set, use it as default sdir
	sdir := os.Getenv("IMMORTAL_SDIR")
	if sdir == "" {
		sdir = "/var/run/immortal"
	}
	return sdir
}

// isDir return true if path is a dir
func isDir(path string) bool {
	f, err := os.Stat(path)
	if err != nil {
		return false
	}
	if m := f.Mode(); m.IsDir() && m&400 != 0 {
		return true
	}
	return false
}

// isFile return true if path is a regular file
func isFile(path string) bool {
	f, err := os.Stat(path)
	if err != nil {
		return false
	}
	if m := f.Mode(); !m.IsDir() && m.IsRegular() && m&400 != 0 {
		return true
	}
	return false
}
