package doc

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"strings"
	"time"
)

// Backend is how the plugin reaches DOC (DOC-SPEC §9). Each call carries the plugin's registration
// token and, while a request or a run is being handled, that call's context token, which is how
// the plugin acts for whoever it is handling.
type Backend struct {
	id      string
	http    *http.Client
	base    string
	token   string
	context string
	caller  *Caller
}

// Refused is the backend, or another plugin, saying no: its status and the problem it gave.
type Refused struct {
	Status int
	Body   string
}

func (r *Refused) Error() string { return fmt.Sprintf("refused with %d: %s", r.Status, r.Body) }

// Detail is the explanation in the problem, or the whole body when it has none.
func (r *Refused) Detail() string {
	var said problem
	if json.Unmarshal([]byte(r.Body), &said) == nil && said.Detail != "" {
		return said.Detail
	}
	return r.Body
}

// IsDuplicate is a key, or unique values, that another record already has.
func IsDuplicate(err error) bool {
	var refused *Refused
	return errors.As(err, &refused) && refused.Status == http.StatusConflict &&
		strings.Contains(refused.Body, "duplicate-record")
}

// For is the backend acting for one call: its context token and its caller.
func (b *Backend) For(contextToken string, caller *Caller) *Backend {
	acting := *b
	acting.context, acting.caller = contextToken, caller
	return &acting
}

// Caller is who the call being handled is for; nil in a scheduled run.
func (b *Backend) Caller() *Caller { return b.caller }

// ID is this plugin's ID.
func (b *Backend) ID() string { return b.id }

// Call posts one request to the backend and reads its answer into out.
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
	raw, err := io.ReadAll(io.LimitReader(resp.Body, 32<<20))
	if err != nil {
		return err
	}
	if resp.StatusCode/100 != 2 {
		return &Refused{Status: resp.StatusCode, Body: string(raw)}
	}
	if out == nil || len(raw) == 0 {
		return nil
	}
	return json.Unmarshal(raw, out)
}

// ---- Data (DOC-SPEC §9.1) ----

// Record is one record, with the backend's own `_created_at`, `_updated_at` and `_version`.
type Record map[string]any

// Text is one of its fields as text.
func (r Record) Text(field string) string {
	if text, ok := r[field].(string); ok {
		return text
	}
	return ""
}

// Number is one of its fields as a number.
func (r Record) Number(field string) float64 {
	if number, ok := r[field].(float64); ok {
		return number
	}
	return 0
}

// Query reads records: those matching Where, newest or oldest first, a page at a time.
type Query struct {
	Collection string         `json:"collection"`
	Where      map[string]any `json:"where,omitempty"`
	Search     string         `json:"search,omitempty"`
	Order      []Order        `json:"order,omitempty"`
	Limit      int            `json:"limit,omitempty"`
	After      string         `json:"after,omitempty"`
}

// Order sorts by the key, `_created_at`, `_updated_at` or the leading fields of an index.
type Order struct {
	Field string `json:"field"`
	Dir   string `json:"dir"` // asc or desc
}

// Newest is the order most pages want.
var Newest = []Order{
	{Field: "_created_at", Dir: "desc"},
}

type dataAnswer struct {
	Record  Record   `json:"record"`
	Records []Record `json:"records"`
	Next    string   `json:"next"`
}

// Data sends one data API request as it is: get, query, aggregate, insert, update, upsert, delete
// or batch.
func (b *Backend) Data(ctx context.Context, request map[string]any, out any) error {
	return b.Call(ctx, "/plugin/v1/data", request, out)
}

// Get is one record by its key, or nil.
func (b *Backend) Get(ctx context.Context, collection string, key any) (Record, error) {
	var answer dataAnswer
	err := b.Data(ctx, map[string]any{"op": "get", "collection": collection, "key": key}, &answer)
	return answer.Record, err
}

// Find is a page of the records a query matches, and where the next page starts.
func (b *Backend) Find(ctx context.Context, query Query) ([]Record, string, error) {
	request := map[string]any{"op": "query", "collection": query.Collection}
	if query.Where != nil {
		request["where"] = query.Where
	}
	if query.Search != "" {
		request["search"] = query.Search
	}
	if len(query.Order) > 0 {
		request["order"] = query.Order
	}
	if query.Limit > 0 {
		request["limit"] = query.Limit
	}
	if query.After != "" {
		request["after"] = query.After
	}
	var answer dataAnswer
	err := b.Data(ctx, request, &answer)
	return answer.Records, answer.Next, err
}

// Insert adds a record and answers it as stored.
func (b *Backend) Insert(ctx context.Context, collection string, values any) (Record, error) {
	var answer dataAnswer
	err := b.Data(ctx, map[string]any{"op": "insert", "collection": collection, "values": values}, &answer)
	return answer.Record, err
}

// Upsert writes a record whole, whether or not it was there, which is what makes a handler safe
// to run twice.
func (b *Backend) Upsert(ctx context.Context, collection string, values any) (Record, error) {
	var answer dataAnswer
	err := b.Data(ctx, map[string]any{"op": "upsert", "collection": collection, "values": values}, &answer)
	return answer.Record, err
}

// Delete removes a record by its key.
func (b *Backend) Delete(ctx context.Context, collection string, key any) error {
	return b.Data(ctx, map[string]any{"op": "delete", "collection": collection, "key": key}, nil)
}

// ---- Events (DOC-SPEC §9.2) ----

// Publish sends an event under plugin.<id>.; name is the rest of the topic, such as
// `cheer.sent`. Publishing to `ui.<page>` refreshes every open page listening for it.
func (b *Backend) Publish(ctx context.Context, name string, payload any) error {
	topic := "plugin." + b.id + "." + name
	return b.Call(ctx, "/plugin/v1/events", map[string]any{"topic": topic, "payload": payload}, nil)
}

// ---- Settings (DOC-SPEC §9.14) ----

// Settings are the plugin's settings as they stand: stored, then the deployment's, then the
// defaults its manifest declares.
type Settings struct {
	Values   map[string]json.RawMessage `json:"values"`
	Secrets  map[string]string          `json:"secrets"`
	Features map[string]bool            `json:"features"`
	// Required settings nobody has set.
	Missing []string `json:"missing"`
}

// Settings reads them. They are cheap to read, so read them where they are used rather than
// keeping a copy that goes stale.
func (b *Backend) Settings(ctx context.Context) (Settings, error) {
	var settings Settings
	err := b.Call(ctx, "/plugin/v1/settings", nil, &settings)
	return settings, err
}

// Text is a setting as text, or "".
func (s Settings) Text(key string) string {
	var text string
	if json.Unmarshal(s.Values[key], &text) == nil {
		return strings.TrimSpace(text)
	}
	return ""
}

// Number is a setting as a number, or fallback.
func (s Settings) Number(key string, fallback float64) float64 {
	var number float64
	if json.Unmarshal(s.Values[key], &number) == nil {
		return number
	}
	return fallback
}

// Bool is a setting as true or false.
func (s Settings) Bool(key string) bool {
	var yes bool
	_ = json.Unmarshal(s.Values[key], &yes)
	return yes
}

// List is a list setting's lines.
func (s Settings) List(key string) []string {
	var lines []string
	_ = json.Unmarshal(s.Values[key], &lines)
	kept := lines[:0]
	for _, line := range lines {
		if line = strings.TrimSpace(line); line != "" {
			kept = append(kept, line)
		}
	}
	return kept
}

// ---- Other plugins, over the Service Bus (DOC-SPEC §9.3) ----

type serviceRequest struct {
	Address    string `json:"address"`
	Subject    string `json:"subject"`
	Payload    any    `json:"payload"`
	DeadlineMS uint64 `json:"deadline_ms,omitempty"`
	Queue      bool   `json:"queue"`
}

type relayed struct {
	Payload struct {
		Status int             `json:"status"`
		Body   json.RawMessage `json:"body"`
	} `json:"payload"`
}

func (b *Backend) relay(ctx context.Context, plugin, subject, method string, body any) (int, json.RawMessage, error) {
	request := serviceRequest{
		Address:    "plugin." + plugin,
		Subject:    subject,
		Payload:    map[string]any{"method": method, "body": body},
		DeadlineMS: uint64((20 * time.Second).Milliseconds()),
	}
	var answer relayed
	if err := b.Call(ctx, "/plugin/v1/services", request, &answer); err != nil {
		return 0, nil, err
	}
	return answer.Payload.Status, answer.Payload.Body, nil
}

// Peer calls another plugin's peer/<route> as this plugin, and answers its status and body.
// Which peer routes a plugin has, and whom they serve, is that plugin's to say.
func (b *Backend) Peer(ctx context.Context, plugin, method, route string, body any) (int, json.RawMessage, error) {
	return b.relay(ctx, plugin, "peer/"+route, method, body)
}

// Ask calls another plugin's api/<route> as whoever this call is for, who needs whatever that
// route needs of them.
func (b *Backend) Ask(ctx context.Context, plugin, method, route string, body any) (int, json.RawMessage, error) {
	return b.relay(ctx, plugin, "api/"+route, method, body)
}

// Queue posts body to another plugin's peer/<route> later, even if it is busy now.
func (b *Backend) Queue(ctx context.Context, plugin, route string, body any) error {
	request := serviceRequest{
		Address: "plugin." + plugin,
		Subject: "peer/" + route,
		Payload: map[string]any{"method": "POST", "body": body},
		Queue:   true,
	}
	return b.Call(ctx, "/plugin/v1/services", request, nil)
}

// Notify puts a notification in a person's DOC inbox, from this plugin. user is their ID in DOC;
// url, which may be "", is where it takes them.
func (b *Backend) Notify(ctx context.Context, user, title, body, url string) error {
	return b.Queue(ctx, "notifications", "notify", map[string]string{
		"user": user, "title": title, "body": body, "url": url,
	})
}
