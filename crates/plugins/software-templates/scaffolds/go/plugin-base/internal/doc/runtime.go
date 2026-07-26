package doc

import (
	"bytes"
	"context"
	"crypto/sha256"
	"crypto/subtle"
	"crypto/tls"
	"crypto/x509"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/quic-go/quic-go"
	"github.com/quic-go/quic-go/http3"
)

// Plugin is everything the plugin is: its manifest and whichever of these it has. Leave out what
// it does not need.
type Plugin struct {
	Manifest Manifest

	// Routes serves the requests forwarded to it, with the path as the plugin sees it: a page at
	// /p/<id>/cheers arrives as GET /ui/cheers, and the API's /api/v1/plugins/<id>/api/cheers as
	// /api/cheers. Another plugin's Peer call arrives under /peer/. From arrives with each one.
	Routes http.Handler

	// Load runs after registering, and after each change of version, with what the version
	// before handed over. Keep it short: nothing is routed here until it returns.
	Load func(ctx context.Context, b *Backend, previous json.RawMessage) error
	// Unload runs before this process hands over to another version. What it answers, under
	// 1 MiB, is the next version's previous.
	Unload func(ctx context.Context, b *Backend) (any, error)
	// Run is what the classification says it is for, and what each schedule starts.
	Run func(ctx context.Context, b *Backend, run Run) (any, error)
	// Cancel asks a run in progress to stop.
	Cancel func(ctx context.Context, b *Backend) error
	// OnEvent is each event the manifest subscribes to. Returning an error has it delivered again.
	OnEvent func(ctx context.Context, b *Backend, event Event) error
	// CheckSettings is the plugin's opinion of settings someone is about to save.
	CheckSettings func(ctx context.Context, b *Backend, proposed Settings) Verdict
}

// From is the backend acting for the request being handled, and who it is for.
func From(r *http.Request) *Backend {
	if b, ok := r.Context().Value(backendKey{}).(*Backend); ok {
		return b
	}
	return nil
}

type backendKey struct{}

// Serve registers the plugin with the backend and serves it until the backend tells it to exit.
func Serve(plugin Plugin) error {
	if plugin.Manifest.Classification == "" {
		plugin.Manifest.Classification = Synchronous
	}
	cfg, err := configured(plugin.Manifest.ID)
	if err != nil {
		return err
	}
	hash, err := binaryHash()
	if err != nil {
		return err
	}
	// Serve as plugin-<id>, trust only the platform's CA, and expect the backend to be "backend".
	cert, err := tls.LoadX509KeyPair(
		filepath.Join(cfg.secrets, "certs", "plugin-"+plugin.Manifest.ID+".pem"),
		filepath.Join(cfg.secrets, "certs", "plugin-"+plugin.Manifest.ID+".key"),
	)
	if err != nil {
		return fmt.Errorf("loading the plugin's certificate: %w", err)
	}
	caPEM, err := os.ReadFile(filepath.Join(cfg.secrets, "ca", "ca.pem"))
	if err != nil {
		return fmt.Errorf("reading the platform's CA: %w", err)
	}
	roots := x509.NewCertPool()
	if !roots.AppendCertsFromPEM(caPEM) {
		return errors.New("the CA file holds no certificate")
	}
	quicConfig := &quic.Config{MaxIdleTimeout: 3 * time.Second, KeepAlivePeriod: time.Second}

	rt := &runtime{
		plugin:  plugin,
		permits: make(chan struct{}, workers),
		exit:    make(chan struct{}),
		backend: &Backend{
			id:    plugin.Manifest.ID,
			base:  "https://" + cfg.backend,
			token: cfg.token,
			http: &http.Client{Transport: &http3.Transport{
				TLSClientConfig: &tls.Config{RootCAs: roots, ServerName: "backend", MinVersion: tls.VersionTLS13},
				QUICConfig:      quicConfig,
			}},
		},
	}
	rt.setState("loading")
	rt.setInstance("")

	server := &http3.Server{
		Addr:       cfg.bind,
		TLSConfig:  http3.ConfigureTLSConfig(&tls.Config{Certificates: []tls.Certificate{cert}, MinVersion: tls.VersionTLS13}),
		QUICConfig: quicConfig,
		Handler:    rt,
	}
	serving := make(chan error, 1)
	go func() { serving <- server.ListenAndServe() }()

	registration := registerRequest{
		Manifest:     plugin.Manifest,
		Address:      cfg.advertise,
		BinarySHA256: hash,
		StartedAt:    time.Now().UTC(),
	}
	ctx, stop := context.WithCancel(context.Background())
	defer stop()
	answer := rt.register(ctx, registration)
	slog.Info("registered with the backend", "plugin", plugin.Manifest.ID, "version", plugin.Manifest.Version, "state", answer.State)
	every := 5 * time.Second
	if answer.LivenessMS > 0 {
		every = time.Duration(answer.LivenessMS) * time.Millisecond
	}
	go rt.liveness(ctx, registration, every)

	select {
	case <-rt.exit:
		// Let the answer to exit leave before the endpoint closes.
		shutdown, cancel := context.WithTimeout(context.Background(), 2*time.Second)
		defer cancel()
		_ = server.Shutdown(shutdown)
		return nil
	case err := <-serving:
		return err
	}
}

// At most this many calls from the backend at once; more answer 429 and are retried.
const workers = 32

type config struct {
	secrets, token, backend, bind, advertise string
}

func env(name, fallback string) string {
	if value := os.Getenv(name); value != "" {
		return value
	}
	return fallback
}

// configured reads where things are from the environment (DOC-SPEC §5.3).
func configured(id string) (config, error) {
	cfg := config{
		secrets: env("DOC_SECRETS_DIR", "/secrets"),
		backend: env("DOC_BACKEND_QUIC", "backend:4433"),
		bind:    env("DOC_PLUGIN_BIND", "0.0.0.0:4440"),
		token:   os.Getenv("DOC_PLUGIN_TOKEN"),
	}
	if cfg.token == "" {
		raw, err := os.ReadFile(filepath.Join(cfg.secrets, "tokens", "plugins", id+".token"))
		if err != nil {
			return cfg, fmt.Errorf("reading the registration token (is %q in the platform's plugins.ids?): %w", id, err)
		}
		cfg.token = strings.TrimSpace(string(raw))
	}
	_, port, err := net.SplitHostPort(cfg.bind)
	if err != nil {
		return cfg, err
	}
	cfg.advertise = env("DOC_PLUGIN_ADVERTISE", net.JoinHostPort(env("HOSTNAME", "plugin-"+id), port))
	return cfg, nil
}

// binaryHash ties a version to the build that first registered it.
func binaryHash() (string, error) {
	path, err := os.Executable()
	if err != nil {
		return "", err
	}
	file, err := os.Open(path)
	if err != nil {
		return "", err
	}
	defer file.Close()
	hash := sha256.New()
	if _, err := io.Copy(hash, file); err != nil {
		return "", err
	}
	return hex.EncodeToString(hash.Sum(nil)), nil
}

// ---- The calls the backend makes: /host/v1/* (DOC-SPEC §7) ----

type runtime struct {
	plugin   Plugin
	backend  *Backend
	secret   atomic.Pointer[string]
	instance atomic.Pointer[string]
	state    atomic.Pointer[string]
	// Held shared by each run, exclusively by load and unload, so they wait for runs to finish.
	runs    sync.RWMutex
	permits chan struct{}
	exit    chan struct{}
	exiting sync.Once
}

func (rt *runtime) setState(state string)       { rt.state.Store(&state) }
func (rt *runtime) getState() string            { return *rt.state.Load() }
func (rt *runtime) setInstance(instance string) { rt.instance.Store(&instance) }

func writeJSON(w http.ResponseWriter, status int, value any) {
	w.Header().Set("content-type", "application/json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(value)
}

// Problem answers an error as the platform does, which is how a route refuses.
func Problem(w http.ResponseWriter, status int, kind, detail string) {
	w.Header().Set("content-type", "application/problem+json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(problem{Type: "/problems/" + kind, Title: kind, Status: status, Detail: detail})
}

// JSON answers a value as JSON.
func JSON(w http.ResponseWriter, status int, value any) { writeJSON(w, status, value) }

func (rt *runtime) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	path := r.URL.EscapedPath()
	if !strings.HasPrefix(path, "/host/v1/") {
		w.WriteHeader(http.StatusNotFound)
		return
	}
	// No secret yet, or the wrong one: an empty 503, which the backend reads as "not ready yet".
	// Nothing else may drive this process.
	secret := rt.secret.Load()
	presented, _ := strings.CutPrefix(r.Header.Get("authorization"), "Bearer ")
	if secret == nil || subtle.ConstantTimeCompare([]byte(presented), []byte(*secret)) != 1 {
		w.WriteHeader(http.StatusServiceUnavailable)
		return
	}
	select {
	case rt.permits <- struct{}{}:
		defer func() { <-rt.permits }()
	default:
		w.WriteHeader(http.StatusTooManyRequests)
		return
	}

	ctx := r.Context()
	if ms, err := strconv.ParseUint(r.Header.Get("x-doc-deadline-ms"), 10, 64); err == nil {
		var cancel context.CancelFunc
		ctx, cancel = context.WithTimeout(ctx, time.Duration(ms)*time.Millisecond)
		defer cancel()
	}
	var caller *Caller
	if raw := r.Header.Get("x-doc-caller"); raw != "" {
		caller = &Caller{}
		if json.Unmarshal([]byte(raw), caller) != nil {
			caller = nil
		}
	}
	b := rt.backend.For(r.Header.Get("x-doc-context"), caller)
	body, err := io.ReadAll(io.LimitReader(r.Body, 16<<20))
	if err != nil {
		Problem(w, 400, "bad-request", err.Error())
		return
	}

	// A panic fails this one call; in load or unload it also puts the plugin in error.
	defer func() {
		if caught := recover(); caught != nil {
			if path == "/host/v1/load" || path == "/host/v1/unload" {
				rt.setState("error")
			}
			slog.Error("a call panicked", "path", path, "panic", caught)
			Problem(w, 500, "panicked", fmt.Sprintf("panicked: %v", caught))
		}
	}()

	switch path {
	case "/host/v1/health":
		writeJSON(w, 200, map[string]string{"id": rt.plugin.Manifest.ID, "version": rt.plugin.Manifest.Version, "state": rt.getState()})
	case "/host/v1/load":
		var in struct {
			Previous json.RawMessage `json:"previous"`
		}
		_ = json.Unmarshal(body, &in)
		rt.setState("loading")
		rt.runs.Lock()
		var err error
		if rt.plugin.Load != nil {
			err = rt.plugin.Load(ctx, b, in.Previous)
		}
		rt.runs.Unlock()
		if err != nil {
			rt.setState("error")
			Problem(w, 500, "load-failed", err.Error())
			return
		}
		rt.setState("running")
		writeJSON(w, 200, map[string]string{"state": "running"})
	case "/host/v1/unload":
		rt.setState("unloading")
		if rt.plugin.Cancel != nil {
			_ = rt.plugin.Cancel(ctx, b)
		}
		rt.runs.Lock()
		var state any
		var err error
		if rt.plugin.Unload != nil {
			state, err = rt.plugin.Unload(ctx, b)
		}
		rt.runs.Unlock()
		if err != nil {
			rt.setState("error")
			Problem(w, 500, "unload-failed", err.Error())
			return
		}
		writeJSON(w, 200, map[string]any{"state": state})
	case "/host/v1/run":
		rt.runs.RLock()
		defer rt.runs.RUnlock()
		if rt.getState() == "unloading" {
			Problem(w, 503, "unavailable", "unloading")
			return
		}
		if rt.plugin.Run == nil {
			writeJSON(w, 200, map[string]any{"payload": nil})
			return
		}
		var in Run
		_ = json.Unmarshal(body, &in)
		out, err := rt.plugin.Run(ctx, b, in)
		if err != nil {
			slog.Warn("a run failed", "error", err)
			Problem(w, 500, "run-failed", err.Error())
			return
		}
		writeJSON(w, 200, map[string]any{"payload": out})
	case "/host/v1/cancel":
		if rt.plugin.Cancel != nil {
			if err := rt.plugin.Cancel(ctx, b); err != nil {
				Problem(w, 500, "cancel-failed", err.Error())
				return
			}
		}
		rt.setState("cancelled")
		writeJSON(w, 200, map[string]string{"state": "cancelled"})
	case "/host/v1/event":
		var event Event
		if err := json.Unmarshal(body, &event); err != nil {
			Problem(w, 400, "bad-request", err.Error())
			return
		}
		if rt.plugin.OnEvent != nil {
			if err := rt.plugin.OnEvent(ctx, b, event); err != nil {
				Problem(w, 500, "event-failed", err.Error())
				return
			}
		}
		writeJSON(w, 200, map[string]bool{"acknowledged": true})
	case "/host/v1/settings/check":
		var proposed Settings
		_ = json.Unmarshal(body, &proposed)
		verdict := Verdict{}
		if rt.plugin.CheckSettings != nil {
			verdict = rt.plugin.CheckSettings(ctx, b, proposed)
		}
		writeJSON(w, 200, verdict)
	case "/host/v1/settings/changed":
		// Settings are read where they are used, so there is nothing to reload.
		writeJSON(w, 200, map[string]bool{"reloaded": false})
	case "/host/v1/exit":
		w.WriteHeader(http.StatusNoContent)
		rt.exiting.Do(func() { close(rt.exit) })
	default:
		route, ok := strings.CutPrefix(path, "/host/v1/request/")
		if !ok {
			Problem(w, 404, "not-found", "no such call")
			return
		}
		if state := rt.getState(); state != "running" && state != "cancelled" {
			Problem(w, 503, "unavailable", state)
			return
		}
		if rt.plugin.Routes == nil {
			Problem(w, 404, "not-found", "no such route")
			return
		}
		// The plugin's own routes see the path as it would be served: /ui/…, /api/…, /peer/….
		forwarded := r.Clone(context.WithValue(ctx, backendKey{}, b))
		forwarded.URL.Path = "/" + route
		forwarded.URL.RawPath = ""
		forwarded.RequestURI = ""
		forwarded.Body = io.NopCloser(bytes.NewReader(body))
		forwarded.ContentLength = int64(len(body))
		rt.plugin.Routes.ServeHTTP(w, forwarded)
	}
}

// ---- Registration and liveness (DOC-SPEC §6) ----

func (rt *runtime) register(ctx context.Context, request registerRequest) registerResponse {
	wait := 500 * time.Millisecond
	for attempt := 1; ; attempt++ {
		var answer registerResponse
		call, cancel := context.WithTimeout(ctx, 10*time.Second)
		err := rt.backend.Call(call, "/plugin/v1/register", request, &answer)
		cancel()
		if err == nil {
			// Stored before anything else: the backend calls load straight away.
			rt.secret.Store(&answer.Secret)
			rt.setInstance(answer.Instance)
			rt.setState(answer.State)
			return answer
		}
		var refused *Refused
		if errors.As(err, &refused) && refused.Status == 400 {
			slog.Error("the backend refused this plugin; fix it and start it again", "problem", refused.Detail())
		} else {
			slog.Warn("registration did not succeed yet", "attempt", attempt, "retry_in", wait, "error", err)
		}
		time.Sleep(wait)
		wait = min(wait*2, 15*time.Second)
	}
}

func (rt *runtime) liveness(ctx context.Context, request registerRequest, every time.Duration) {
	for {
		select {
		case <-ctx.Done():
			return
		case <-time.After(every):
		}
		report := liveness{ID: request.Manifest.ID, State: rt.getState(), Instance: *rt.instance.Load()}
		call, cancel := context.WithTimeout(ctx, every)
		err := rt.backend.Call(call, "/plugin/v1/liveness", report, nil)
		cancel()
		var refused *Refused
		switch {
		case err == nil:
		case errors.As(err, &refused) && (refused.Status == 410 || (refused.Status == 404 && report.State == "unloading")):
			slog.Info("the backend has moved on from this process; exiting")
			rt.exiting.Do(func() { close(rt.exit) })
			return
		case errors.As(err, &refused) && refused.Status == 404:
			slog.Info("the backend has no record of this plugin; registering again")
			rt.secret.Store(nil)
			rt.register(ctx, request)
		default:
			slog.Debug("a liveness report did not reach the backend", "error", err)
		}
	}
}
