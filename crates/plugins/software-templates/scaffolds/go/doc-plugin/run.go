package main

import (
	"context"
	"encoding/json"
	"log/slog"
	"sync"
	"time"

	"{{ scaffold.module }}/internal/doc"
)

// load runs when DOC loads this version, with what the version before it handed over from its
// unload. Keep it short: nothing is routed here until it returns.
func load(ctx context.Context, b *doc.Backend, previous json.RawMessage) error {
	slog.Info("loaded", "plugin", b.ID(), "version", version)
	return nil
}

// work is the run in progress, which cancel stops.
var work struct {
	sync.Mutex
	stop chan struct{}
}

// run is what the classification says it is for: on demand for a synchronous or async plugin,
// once after each load for a one-shot one, and for as long as it is loaded for a long-running
// one. Each schedule in the manifest runs it too, with started.Schedule() naming it.
func run(ctx context.Context, b *doc.Backend, started doc.Run) (any, error) {
	if manifest().Classification != doc.LongRunning {
		slog.Info("ran", "schedule", started.Schedule())
		return map[string]string{"ran": time.Now().UTC().Format(time.RFC3339)}, nil
	}
	// A long-running plugin's run is expected to keep going: DOC treats it ending as an error.
	work.Lock()
	stop := make(chan struct{})
	work.stop = stop
	work.Unlock()
	every := time.NewTicker(time.Minute)
	defer every.Stop()
	for {
		select {
		case <-ctx.Done():
			return nil, ctx.Err()
		case <-stop:
			return map[string]string{"stopped": time.Now().UTC().Format(time.RFC3339)}, nil
		case <-every.C:
			slog.Debug("still running")
		}
	}
}

// cancel asks a run in progress to stop: when an administrator cancels the plugin, and before
// this version hands over to another.
func cancel(ctx context.Context, b *doc.Backend) error {
	work.Lock()
	defer work.Unlock()
	if work.stop != nil {
		close(work.stop)
		work.stop = nil
	}
	return nil
}

// onEvent is each event the manifest subscribes to, delivered at least once: handling one twice
// must do no harm. Returning an error has it delivered again later.
func onEvent(ctx context.Context, b *doc.Backend, event doc.Event) error {
	slog.Info("heard", "topic", event.Topic, "id", event.ID)
	return nil
}
