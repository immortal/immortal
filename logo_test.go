package immortal

import (
	"testing"
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
