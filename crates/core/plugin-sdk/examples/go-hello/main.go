// A DOC plugin in Go.
// Everything a plugin must do is here, so it can be read top to bottom against DOC-SPEC.md.
//
// It stores its greetings through the data API, in a collection its manifest declares.
// It needs the plugin ID `hello-go` in `plugins.ids` in config/doc.toml, and
// the bootstrap job ran to issue a secret from core for the plugin.
//
//	go build -o hello-go . && DOC_SECRETS_DIR=../../../../../secrets \
//	  DOC_BACKEND_QUIC=127.0.0.1:4433 DOC_PLUGIN_BIND=127.0.0.1:4450 \
//	  DOC_PLUGIN_ADVERTISE=127.0.0.1:4450 ./hello-go
package main

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
	"html"
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

const (
	pluginID = "hello-go"
	version  = "1.0.0" // Set from your build, e.g. -ldflags "-X main.version=...".
	workers  = 32
)

// Protocol types (DOC-SPEC.md #4 and #9)
type Manifest struct {
	ID                string             `json:"id"`
	Version           string             `json:"version"`
	Classification    string             `json:"classification"`
	CustomPermissions []CustomPermission `json:"custom_permissions,omitempty"`
	Nav               []Nav              `json:"nav,omitempty"`
	Subscriptions     []string           `json:"subscriptions,omitempty"`
	Data              json.RawMessage    `json:"data,omitempty"`
}

type CustomPermission struct {
	Kind string `json:"kind"` // "plugin-user" or "plugin-service"
	Name string `json:"name"`
}

type Nav struct {
	Label       string `json:"label"`
	Path        string `json:"path"`
	Description string `json:"description,omitempty"` // shown on the landing page's card
}

type registerRequest struct {
	Manifest     Manifest  `json:"manifest"`
	Address      string    `json:"address"`
	BinarySHA256 string    `json:"binary_sha256"`
	StartedAt    time.Time `json:"started_at"`
}

type registerResponse struct {
	Secret     string          `json:"secret"`
	Previous   json.RawMessage `json:"previous"`
	State      string          `json:"state"`
	LivenessMS uint64          `json:"liveness_ms"`
	Instance   string          `json:"instance"`
}

type liveness struct {
	ID       string  `json:"id"`
	State    string  `json:"state"`
	Error    *string `json:"error"`
	Instance string  `json:"instance,omitempty"`
}

type Caller struct {
	Kind       string            `json:"kind"`
	ID         string            `json:"id"`
	Label      string            `json:"label"`
	Admin      bool              `json:"admin"`
	Custom     map[string]string `json:"custom"`
	Attributes map[string]string `json:"attributes"`
}

// Allows is the plugin's own check of its custom permissions. Core has already checked
// plugin:<id>:user or :service before forwarding the request.
func (c *Caller) Allows(permission string, write bool) bool {
	if c == nil {
		return false
	}
	if c.Admin {
		return true
	}
	switch c.Custom[permission] {
	case "rw":
		return true
	case "ro":
		return !write
	case "wo":
		return write
	}
	return false
}

type Event struct {
	ID            string          `json:"id"`
	Topic         string          `json:"topic"`
	Source        string          `json:"source"`
	At            time.Time       `json:"at"`
	CorrelationID *string         `json:"correlation_id"`
	SchemaVersion *uint32         `json:"schema_version"`
	Payload       json.RawMessage `json:"payload"`
}

type runInput struct {
	Task    *string         `json:"task"`
	Payload json.RawMessage `json:"payload"`
}

type problem struct {
	Type   string `json:"type"`
	Title  string `json:"title"`
	Status int    `json:"status"`
	Detail string `json:"detail,omitempty"`
}

// Configuration (DOC-SPEC.md #5.3)
type config struct {
	secrets   string
	token     string
	backend   string
	bind      string
	advertise string
}

func env(name, fallback string) string {
	if v := os.Getenv(name); v != "" {
		return v
	}
	return fallback
}

func loadConfig() (config, error) {
	c := config{
		secrets: env("DOC_SECRETS_DIR", "/secrets"),
		backend: env("DOC_BACKEND_QUIC", "backend:4433"),
		bind:    env("DOC_PLUGIN_BIND", "0.0.0.0:4440"),
	}
	c.token = os.Getenv("DOC_PLUGIN_TOKEN")
	if c.token == "" {
		raw, err := os.ReadFile(filepath.Join(c.secrets, "tokens/plugins", pluginID+".token"))
		if err != nil {
			return c, fmt.Errorf("reading the registration token: %w", err)
		}
		c.token = strings.TrimSpace(string(raw))
	}
	_, port, err := net.SplitHostPort(c.bind)
	if err != nil {
		return c, err
	}
	c.advertise = env("DOC_PLUGIN_ADVERTISE", net.JoinHostPort(env("HOSTNAME", "plugin-"+pluginID), port))
	return c, nil
}

func binaryHash() (string, error) {
	path, err := os.Executable()
	if err != nil {
		return "", err
	}
	f, err := os.Open(path)
	if err != nil {
		return "", err
	}
	defer f.Close()
	h := sha256.New()
	if _, err := io.Copy(h, f); err != nil {
		return "", err
	}
	return hex.EncodeToString(h.Sum(nil)), nil
}

// Backend client (DOC-SPEC.md #9)
type refused struct {
	status int
	body   string
}

func (r *refused) Error() string { return fmt.Sprintf("the backend refused: %d %s", r.status, r.body) }

type Backend struct {
	http    *http.Client
	base    string
	token   string
	context string // the x-doc-context of the call being handled, if any
	caller  *Caller
}

// Returns a Backend that acts for the call being handled.
func (b *Backend) For(contextToken string, caller *Caller) *Backend {
	c := *b
	c.context, c.caller = contextToken, caller
	return &c
}

func (b *Backend) Call(ctx context.Context, path string, in, out any) error {
	body, err := json.Marshal(in)
	if err != nil {
		return err
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, b.base+path, bytes.NewReader(body))
	if err != nil {
		return err
	}
	req.Header.Set("content-type", "application/json")
	req.Header.Set("authorization", "Bearer "+b.token)
	if b.context != "" {
		req.Header.Set("x-doc-context", b.context)
	}
	resp, err := b.http.Do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	raw, err := io.ReadAll(resp.Body)
	if err != nil {
		return err
	}
	if resp.StatusCode/100 != 2 {
		return &refused{status: resp.StatusCode, body: string(raw)}
	}
	if out == nil || len(raw) == 0 {
		return nil
	}
	return json.Unmarshal(raw, out)
}

// Data sends one data API request (#9.1): get, query, aggregate, insert, update, upsert, delete, batch
func (b *Backend) Data(ctx context.Context, request map[string]any, out any) error {
	return b.Call(ctx, "/plugin/v1/data", request, out)
}

func (b *Backend) Insert(ctx context.Context, collection string, values any) (map[string]any, error) {
	var out struct {
		Record map[string]any `json:"record"`
	}
	err := b.Data(ctx, map[string]any{"op": "insert", "collection": collection, "values": values}, &out)
	return out.Record, err
}

func (b *Backend) Publish(ctx context.Context, topic string, payload any) error {
	return b.Call(ctx, "/plugin/v1/events", map[string]any{"topic": topic, "payload": payload}, nil)
}

// The plugin
// dataDeclaration is the manifest's `data` (#4.3)
const dataDeclaration = `{
  "collections": {
    "greetings": {
      "fields": {
        "id":   { "type": "uuid", "key": true },
        "name": { "type": "text", "required": true, "max": 200 },
        "at":   { "type": "timestamp", "required": true, "default": "now" }
      },
      "indexes": [["at"]],
      "search": ["name"]
    }
  }
}`

type hello struct {
	greeted atomic.Uint64
}

func (h *hello) load(ctx context.Context, b *Backend, previous json.RawMessage) error {
	var carried struct {
		Greeted uint64 `json:"greeted"`
	}
	if len(previous) > 0 && string(previous) != "null" {
		if err := json.Unmarshal(previous, &carried); err != nil {
			return fmt.Errorf("reading the handed-over state: %w", err)
		}
	}
	h.greeted.Store(carried.Greeted)
	return nil
}

func (h *hello) unload(ctx context.Context, b *Backend) (any, error) {
	return map[string]any{"greeted": h.greeted.Load()}, nil
}

func (h *hello) run(ctx context.Context, b *Backend, in runInput) (any, error) {
	var args struct {
		Name string `json:"name"`
	}
	_ = json.Unmarshal(in.Payload, &args)
	if args.Name == "" {
		args.Name = "world"
	}
	h.greeted.Add(1)
	if _, err := b.Insert(ctx, "greetings", map[string]string{"name": args.Name}); err != nil {
		return nil, err
	}
	_ = b.Publish(ctx, "plugin."+pluginID+".greeted", map[string]string{"name": args.Name})
	return map[string]string{"greeting": "hello, " + args.Name}, nil
}

func (h *hello) cancel(ctx context.Context, b *Backend) error { return nil }

func (h *hello) onEvent(ctx context.Context, b *Backend, ev Event) error {
	slog.Info("event", "topic", ev.Topic, "id", ev.ID)
	return nil
}

// handle serves api/*, ui/*, public/* and internal/*. path has no leading slash.
func (h *hello) handle(ctx context.Context, b *Backend, w http.ResponseWriter, r *http.Request, path string) {
	switch {
	case r.Method == http.MethodGet && (path == "ui" || path == "ui/"):
		w.Header().Set("content-type", "text/html; charset=utf-8")
		fmt.Fprintf(w, `<div class="doc-card"><h3 class="doc-card__heading">Hello from Go</h3>`+
			`<div class="doc-card__content"><p>%d greetings so far, %s.</p></div></div>`,
			h.greeted.Load(), html.EscapeString(label(b.caller)))
	case r.Method == http.MethodGet && path == "api/greetings":
		writeJSON(w, 200, map[string]uint64{"greeted": h.greeted.Load()})
	case r.Method == http.MethodPost && path == "api/greetings":
		if !b.caller.Allows("greetings", true) {
			writeProblem(w, 403, "forbidden", "needs plugin:"+pluginID+":pluginuser:greetings:rw")
			return
		}
		out, err := h.run(ctx, b, runInput{Payload: json.RawMessage(`{}`)})
		if err != nil {
			writeProblem(w, 500, "failed", err.Error())
			return
		}
		writeJSON(w, 201, out)
	default:
		writeProblem(w, 404, "not-found", "no such route")
	}
}

func label(c *Caller) string {
	if c == nil || c.Label == "" {
		return "stranger"
	}
	return c.Label
}

// Runtime: /host/v1/* (DOC-SPEC.md #7)
type runtime struct {
	plugin   *hello
	backend  *Backend
	secret   atomic.Pointer[string]
	instance atomic.Pointer[string]
	state    atomic.Pointer[string]
	runs     sync.RWMutex
	permits  chan struct{}
	exit     chan struct{}
	exiting  sync.Once
}

func (rt *runtime) setState(s string) { rt.state.Store(&s) }
func (rt *runtime) getState() string  { return *rt.state.Load() }

func writeJSON(w http.ResponseWriter, status int, v any) {
	w.Header().Set("content-type", "application/json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(v)
}

func writeProblem(w http.ResponseWriter, status int, kind, detail string) {
	w.Header().Set("content-type", "application/problem+json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(problem{Type: "/problems/" + kind, Title: kind, Status: status, Detail: detail})
}

func (rt *runtime) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	path := r.URL.EscapedPath()
	if !strings.HasPrefix(path, "/host/v1/") {
		w.WriteHeader(http.StatusNotFound)
		return
	}
	// No secret yet, or the wrong one: 503 with an empty body on purpose - nothing else controls this process
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
		writeProblem(w, 400, "bad-request", err.Error())
		return
	}

	// A panic fails this one call. In load or unload it also puts the plugin in error.
	defer func() {
		if p := recover(); p != nil {
			if path == "/host/v1/load" || path == "/host/v1/unload" {
				rt.setState("error")
			}
			writeProblem(w, 500, "panicked", fmt.Sprintf("panicked: %v", p))
		}
	}()

	switch path {
	case "/host/v1/health":
		writeJSON(w, 200, map[string]string{"id": pluginID, "version": version, "state": rt.getState()})
	case "/host/v1/load":
		var in struct {
			Previous json.RawMessage `json:"previous"`
		}
		_ = json.Unmarshal(body, &in)
		rt.setState("loading")
		rt.runs.Lock()
		err := rt.plugin.load(ctx, b, in.Previous)
		rt.runs.Unlock()
		if err != nil {
			rt.setState("error")
			writeProblem(w, 500, "load-failed", err.Error())
			return
		}
		rt.setState("running")
		writeJSON(w, 200, map[string]string{"state": "running"})
	case "/host/v1/unload":
		rt.setState("unloading")
		_ = rt.plugin.cancel(ctx, b) // ask any run in progress to stop
		rt.runs.Lock()
		state, err := rt.plugin.unload(ctx, b)
		rt.runs.Unlock()
		if err != nil {
			rt.setState("error")
			writeProblem(w, 500, "unload-failed", err.Error())
			return
		}
		writeJSON(w, 200, map[string]any{"state": state})
	case "/host/v1/run":
		rt.runs.RLock()
		defer rt.runs.RUnlock()
		if rt.getState() == "unloading" {
			writeProblem(w, 503, "unavailable", "unloading")
			return
		}
		var in runInput
		_ = json.Unmarshal(body, &in)
		out, err := rt.plugin.run(ctx, b, in)
		if err != nil {
			writeProblem(w, 500, "run-failed", err.Error())
			return
		}
		writeJSON(w, 200, map[string]any{"payload": out})
	case "/host/v1/cancel":
		if err := rt.plugin.cancel(ctx, b); err != nil {
			writeProblem(w, 500, "cancel-failed", err.Error())
			return
		}
		rt.setState("cancelled")
		writeJSON(w, 200, map[string]string{"state": "cancelled"})
	case "/host/v1/event":
		var ev Event
		if err := json.Unmarshal(body, &ev); err != nil {
			writeProblem(w, 400, "bad-request", err.Error())
			return
		}
		if err := rt.plugin.onEvent(ctx, b, ev); err != nil {
			writeProblem(w, 500, "event-failed", err.Error())
			return
		}
		writeJSON(w, 200, map[string]bool{"acknowledged": true})
	case "/host/v1/exit":
		w.WriteHeader(http.StatusNoContent)
		rt.exiting.Do(func() { close(rt.exit) })
	default:
		route, ok := strings.CutPrefix(path, "/host/v1/request/")
		if !ok {
			writeProblem(w, 404, "not-found", "no such route")
			return
		}
		if s := rt.getState(); s != "running" && s != "cancelled" {
			writeProblem(w, 503, "unavailable", s)
			return
		}
		r.Body = io.NopCloser(bytes.NewReader(body))
		rt.plugin.handle(ctx, b, w, r, route)
	}
}

// Registration and liveness (DOC-SPEC.md #6)
func (rt *runtime) register(ctx context.Context, req registerRequest) registerResponse {
	wait := 500 * time.Millisecond
	for attempt := 1; ; attempt++ {
		var resp registerResponse
		callCtx, cancel := context.WithTimeout(ctx, 10*time.Second)
		err := rt.backend.Call(callCtx, "/plugin/v1/register", req, &resp)
		cancel()
		if err == nil {
			// Store before anything else: backend dials /host/v1/load immediately
			rt.secret.Store(&resp.Secret)
			rt.instance.Store(&resp.Instance)
			rt.setState(resp.State)
			return resp
		}
		slog.Warn("registration did not succeed yet", "attempt", attempt, "retry_in", wait, "error", err)
		time.Sleep(wait)
		wait = min(wait*2, 15*time.Second)
	}
}

func (rt *runtime) liveness(ctx context.Context, req registerRequest, every time.Duration) {
	for {
		select {
		case <-ctx.Done():
			return
		case <-time.After(every):
		}
		report := liveness{ID: pluginID, State: rt.getState(), Instance: *rt.instance.Load()}
		callCtx, cancel := context.WithTimeout(ctx, every)
		err := rt.backend.Call(callCtx, "/plugin/v1/liveness", report, nil)
		cancel()
		var r *refused
		switch {
		case err == nil:
		case errors.As(err, &r) && (r.status == 410 || (r.status == 404 && report.State == "unloading")):
			slog.Info("the backend has moved on from this process; exiting")
			rt.exiting.Do(func() { close(rt.exit) })
			return
		case errors.As(err, &r) && r.status == 404:
			slog.Info("the backend has no record of this plugin; registering again")
			rt.secret.Store(nil)
			rt.register(ctx, req)
		default:
			slog.Debug("a liveness report did not reach the backend", "error", err)
		}
	}
}

func main() {
	if err := run(); err != nil {
		slog.Error("the plugin stopped", "error", err)
		os.Exit(1)
	}
}

func run() error {
	cfg, err := loadConfig()
	if err != nil {
		return err
	}
	hash, err := binaryHash()
	if err != nil {
		return err
	}

	// TLS: serve as plugin-<id>, trust only the bootstrap CA, dial the backend as "backend".
	cert, err := tls.LoadX509KeyPair(
		filepath.Join(cfg.secrets, "certs", "plugin-"+pluginID+".pem"),
		filepath.Join(cfg.secrets, "certs", "plugin-"+pluginID+".key"),
	)
	if err != nil {
		return fmt.Errorf("loading the plugin certificate: %w", err)
	}
	caPEM, err := os.ReadFile(filepath.Join(cfg.secrets, "ca", "ca.pem"))
	if err != nil {
		return fmt.Errorf("reading the CA: %w", err)
	}
	roots := x509.NewCertPool()
	if !roots.AppendCertsFromPEM(caPEM) {
		return errors.New("the CA file holds no certificate")
	}
	quicConfig := &quic.Config{MaxIdleTimeout: 3 * time.Second, KeepAlivePeriod: time.Second}

	rt := &runtime{
		plugin:  &hello{},
		permits: make(chan struct{}, workers),
		exit:    make(chan struct{}),
		backend: &Backend{
			base:  "https://" + cfg.backend,
			token: cfg.token,
			http: &http.Client{Transport: &http3.Transport{
				TLSClientConfig: &tls.Config{RootCAs: roots, ServerName: "backend", MinVersion: tls.VersionTLS13},
				QUICConfig:      quicConfig,
			}},
		},
	}
	rt.setState("loading")
	empty := ""
	rt.instance.Store(&empty)

	server := &http3.Server{
		Addr:       cfg.bind,
		TLSConfig:  http3.ConfigureTLSConfig(&tls.Config{Certificates: []tls.Certificate{cert}, MinVersion: tls.VersionTLS13}),
		QUICConfig: quicConfig,
		Handler:    rt,
	}
	serveErr := make(chan error, 1)
	go func() { serveErr <- server.ListenAndServe() }()

	manifest := Manifest{
		ID:                pluginID,
		Version:           version,
		Classification:    "synchronous",
		CustomPermissions: []CustomPermission{{Kind: "plugin-user", Name: "greetings"}},
		Nav:               []Nav{{Label: "Hello (Go)", Path: "/", Description: "The example plugin, written in Go"}},
		Subscriptions:     []string{"platform.plugin.>"},
		Data:              json.RawMessage(dataDeclaration),
	}
	req := registerRequest{Manifest: manifest, Address: cfg.advertise, BinarySHA256: hash, StartedAt: time.Now().UTC()}

	ctx, stop := context.WithCancel(context.Background())
	defer stop()
	resp := rt.register(ctx, req)
	slog.Info("registered with the backend", "state", resp.State)
	every := 5 * time.Second
	if resp.LivenessMS > 0 {
		every = time.Duration(resp.LivenessMS) * time.Millisecond
	}
	go rt.liveness(ctx, req, every)

	select {
	case <-rt.exit:
		// 204 needs to leave before proc exit
		shutdown, cancel := context.WithTimeout(context.Background(), 2*time.Second)
		defer cancel()
		_ = server.Shutdown(shutdown)
		return nil
	case err := <-serveErr:
		return err
	}
}
