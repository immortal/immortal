package immortal

import "testing"

func TestProcessStart(t *testing.T) {
	cfg := &Config{
		command: []string{"--"},
	}

	quit := make(chan struct{})
	np := &process{
		Config: cfg,
		Logger: &LogWriter{
			logger: NewLogger(cfg, quit),
		},
		LoggerStderr: &LogWriter{
			logger: NewStderrLogger(cfg),
		},
		quit: quit,
	}
	_, err := np.Start()
	if err == nil {
		t.Error("Expecting exec: --: executable file not found in $PATH")
	}
}
