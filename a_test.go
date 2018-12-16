package immortal

import (
	"bytes"
	"encoding/json"
	"fmt"
	"os"
	"os/signal"
	"path/filepath"
	"reflect"
	"runtime"
	"sync"
	"syscall"
	"testing"
	"time"
)

func TestMain(m *testing.M) {
	switch os.Getenv("GO_WANT_HELPER_PROCESS") {
	case "sleep10":
		GWHPsleep10()
	case "nosleep":
		GWHPnosleep()
	case "signalsUDOT":
		GWHPsignalsUDOT()
	case "logStdoutStderr":
		GWHPlogstdoutstderr()
	case "signalsFiFo":
		GWHPsignalsFiFo()
	case "logSIGPIPE":
		GWHPlogSIGPIPE()
	default:
		os.Exit(m.Run())
	}
}

// GWHPsleep1 - test function, exit 1 after sleeping 10 seconds
func GWHPsleep10() {
	c := make(chan os.Signal)
	signal.Notify(c, os.Interrupt, os.Kill)
	select {
	case <-c:
		os.Exit(0)
	case <-time.After(10 * time.Second):
		os.Exit(1)
	}
}

// GWHPnosleep - test function, exit immediately
func GWHPnosleep() {
	os.Exit(0)
}

// GWHPsignalsUDOT - test function for signals up, down, once, terminate
func GWHPsignalsUDOT() {
	fmt.Println("5D675098-45D7-4089-A72C-3628713EA5BA")
	c := make(chan os.Signal)
	signal.Notify(c, os.Interrupt, os.Kill)
	select {
	case <-c:
		os.Exit(0)
	case <-time.After(10 * time.Second):
		os.Exit(1)
	}
}

// GWHPlogstdoutstderr - test function for login stdout & stderr
func GWHPlogstdoutstderr() {
	for i := 1; i < 5; i++ {
		if i%3 == 0 {
			fmt.Fprintf(os.Stderr, "STDERR i: %d\n", i)
		} else {
			fmt.Printf("STDOUT i: %d\n", i)
		}
	}
}

// GWHPsignalsFiFo - test signals via pipe
func GWHPsignalsFiFo() {
	tmpdir := os.Getenv("TEST_TEMP_DIR")
	c := make(chan os.Signal, 1)
	signal.Notify(c,
		syscall.SIGALRM,
		syscall.SIGCONT,
		syscall.SIGHUP,
		syscall.SIGINT,
		syscall.SIGQUIT,
		syscall.SIGTTIN,
		syscall.SIGTTOU,
		syscall.SIGUSR1,
		syscall.SIGUSR2,
		syscall.SIGWINCH,
	)
	fifo, err := OpenFifo(filepath.Join(tmpdir, "fifo"))
	if err != nil {
		panic(err)
	}
	defer fifo.Close()
	for {
		signalType := <-c
		switch signalType {
		case syscall.SIGALRM:
			fmt.Fprintln(fifo, "--a")
		case syscall.SIGCONT:
			fmt.Fprintln(fifo, "--c")
		case syscall.SIGHUP:
			fmt.Fprintln(fifo, "--h")
		case syscall.SIGINT:
			fmt.Fprintln(fifo, "--i")
		case syscall.SIGQUIT:
			fmt.Fprintln(fifo, "--q")
		case syscall.SIGTTIN:
			fmt.Fprintln(fifo, "--in")
		case syscall.SIGTTOU:
			fmt.Fprintln(fifo, "--ou")
		case syscall.SIGUSR1:
			fmt.Fprintln(fifo, "--1")
		case syscall.SIGUSR2:
			fmt.Fprintln(fifo, "--2")
		case syscall.SIGWINCH:
			fmt.Fprintln(fifo, "--w")
		}
	}
}

// GWHPlogSIGPIPE - test Log to prevent a SIGPIPE
func GWHPlogSIGPIPE() {
	i := 0
	for {
		fmt.Println(i)
		i++
		time.Sleep(time.Millisecond)
		if i%3 == 0 {
			buf := make([]byte, 1000*1000)
			fmt.Printf("%v", buf)
		}
	}
}

// MakeFifo creates a fifo file
func MakeFifo(path string) error {
	err := os.MkdirAll(filepath.Dir(path), os.ModePerm)
	if err != nil {
		return err
	}
	err = syscall.Mknod(path, syscall.S_IFIFO|0666, 0)
	// ignore "file exists" errors and assume the FIFO was pre-made
	if err != nil && !os.IsExist(err) {
		return err
	}
	return nil
}

// OpenFifo open fifo and returns its file descriptor
func OpenFifo(path string) (*os.File, error) {
	f, err := os.OpenFile(path, os.O_RDWR, os.ModeNamedPipe)
	if err != nil {
		return nil, err
	}
	return f, nil
}

/* Test Helpers */
func expect(t *testing.T, a interface{}, b interface{}) {
	_, fn, line, _ := runtime.Caller(1)
	if a != b {
		t.Fatalf("Expected: %v (type %v)  Got: %v (type %v)  in %s:%d", a, reflect.TypeOf(a), b, reflect.TypeOf(b), fn, line)
	}
}

/* prettyPrint */
func prettyPrint(i interface{}) string {
	s, _ := json.MarshalIndent(i, "", "\t")
	return string(s)
}

// myBuffer - bytes.Buffer thread-safe
type myBuffer struct {
	b bytes.Buffer
	sync.RWMutex
}

func (b *myBuffer) Read(p []byte) (n int, err error) {
	b.RLock()
	defer b.RUnlock()
	return b.b.Read(p)
}
func (b *myBuffer) Write(p []byte) (n int, err error) {
	b.Lock()
	defer b.Unlock()
	return b.b.Write(p)
}
func (b *myBuffer) String() string {
	b.RLock()
	defer b.RUnlock()
	return b.b.String()
}
func (b *myBuffer) Reset() {
	b.Lock()
	defer b.Unlock()
	b.b.Reset()
}
