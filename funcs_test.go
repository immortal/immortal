package immortal

import (
	"testing"
	"time"
)

func TestLogo(t *testing.T) {
	logo := Logo()
	if logo != 11093 {
		t.Errorf("Expecting: 11093 got: %v", logo)
	}
}

func TestIconOk(t *testing.T) {
	i := Icon("1F621")
	if i != 128545 {
		t.Errorf("Expecting: 128545 got: %v", i)
	}
}

func TestIconErr(t *testing.T) {
	i := Icon(" ")
	if i != 0 {
		t.Errorf("Expecting: 0 got: %v", i)
	}
}

func TestAbsSince(t *testing.T) {
	start := time.Unix(0, 0)
	diff := AbsSince(start)
	if len(diff) < 16 {
		t.Errorf("Check that systems clock are in sync, diff: %s", diff)
	}
}
