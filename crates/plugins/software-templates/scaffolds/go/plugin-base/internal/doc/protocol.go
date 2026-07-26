// Package doc speaks DOC's plugin protocol, version 1, so the rest of this repository can be the
// plugin itself. It is one of DOC's Go plugin kits: its sections follow DOC-SPEC.md, which is the
// reference for everything here, and nothing in it needs changing to write a plugin.
package doc

import (
	"encoding/json"
	"time"
)

// Manifest is what a plugin tells the backend about itself when it registers (DOC-SPEC §4). The
// backend builds the navigation, the Settings page, the collections and the schedules from it.
type Manifest struct {
	// 1 to 32 lowercase letters, digits and dashes, starting with a letter. It must be listed in
	// the platform's `plugins.ids`, and it is how every other plugin and person addresses this one.
	ID      string `json:"id"`
	Version string `json:"version"`
	// synchronous, async, one-shot or long-running: what `run` is for.
	Classification    string             `json:"classification"`
	CustomPermissions []CustomPermission `json:"custom_permissions,omitempty"`
	Nav               []Nav              `json:"nav,omitempty"`
	// Topic filters delivered to OnEvent, such as `plugin.github.pull-request.merged`.
	Subscriptions []string   `json:"subscriptions,omitempty"`
	Schedules     []Schedule `json:"schedules,omitempty"`
	Settings      []Setting  `json:"settings,omitempty"`
	// What automations can call here by name, and what anything discovering this plugin can read
	// about its API.
	Operations []Operation `json:"operations,omitempty"`
	Data       *Data       `json:"data,omitempty"`
}

// Classifications.
const (
	Synchronous = "synchronous"
	Async       = "async"
	OneShot     = "one-shot"
	LongRunning = "long-running"
)

// CustomPermission is a permission the plugin checks itself, with Caller.Allows.
type CustomPermission struct {
	Kind string `json:"kind"` // "plugin-user" or "plugin-service"
	Name string `json:"name"`
}

// Nav is a link in the navigation, shown to whoever can read the plugin.
type Nav struct {
	Label string `json:"label"`
	// Under the plugin's pages: "/" is ui/, shown at /p/<id>/.
	Path string `json:"path"`
	// One sentence, shown on the landing page's card.
	Description string `json:"description,omitempty"`
	// The menu it sits in, such as Technology or Admin; without one it stands alone in the bar.
	Group string `json:"group,omitempty"`
}

// Schedule has doc-workers queue a run of the plugin, with {"schedule": Name}, whenever the cron
// expression (in UTC) comes round.
type Schedule struct {
	Name        string `json:"name"`
	Cron        string `json:"cron"`
	Description string `json:"description,omitempty"`
	// A `cron` setting whose value replaces Cron when it is set.
	Setting string `json:"setting,omitempty"`
}

// Setting is one field on the plugin's Settings page. The backend checks a value against it before
// it is stored, so the plugin never sees a number where it declared a URL.
type Setting struct {
	Key   string `json:"key"`
	Label string `json:"label,omitempty"`
	Hint  string `json:"hint,omitempty"`
	Group string `json:"group,omitempty"`
	// text, number, boolean, choice, list, url, duration, cron or secret.
	Kind     string   `json:"kind"`
	Default  any      `json:"default,omitempty"`
	Required bool     `json:"required,omitempty"`
	Min      *float64 `json:"min,omitempty"`
	Max      *float64 `json:"max,omitempty"`
	OneOf    []string `json:"one_of,omitempty"`
	Pattern  string   `json:"pattern,omitempty"`
}

// Operation is a call automations can make by name: a route under api/ and what it takes.
type Operation struct {
	Name        string           `json:"name"`
	Label       string           `json:"label"`
	Description string           `json:"description,omitempty"`
	Method      string           `json:"method"`
	Route       string           `json:"route"`
	Params      []OperationParam `json:"params,omitempty"`
}

// OperationParam is one value an operation is called with.
type OperationParam struct {
	Name     string `json:"name"`
	Label    string `json:"label"`
	Hint     string `json:"hint,omitempty"`
	Required bool   `json:"required,omitempty"`
}

// Data declares the plugin's collections (DOC-SPEC §4.3). The backend creates them, and changes
// them when a new version declares something new.
type Data struct {
	Collections map[string]Collection `json:"collections"`
}

// Collection is one kind of record the plugin keeps.
type Collection struct {
	Fields map[string]Field `json:"fields"`
	// Fields a query may filter or sort on together.
	Indexes [][]string `json:"indexes,omitempty"`
	// Fields searched with full text.
	Search []string `json:"search,omitempty"`
}

// Field is one field of a collection: text, integer, number, boolean, timestamp, date, uuid,
// json or list.
type Field struct {
	Type     string   `json:"type"`
	Key      bool     `json:"key,omitempty"`
	Required bool     `json:"required,omitempty"`
	Default  any      `json:"default,omitempty"`
	Max      *float64 `json:"max,omitempty"`
}

// Limit is for a Setting's or a Field's Min and Max.
func Limit(value float64) *float64 { return &value }

// Caller is who a forwarded request, or the call being handled, is for.
type Caller struct {
	// user, service, plugin, platform or anonymous.
	Kind  string `json:"kind"`
	ID    string `json:"id"`
	Label string `json:"label"`
	// A platform admin passes every check.
	Admin bool `json:"admin"`
	// Their own scope on this plugin: ro, rw or wo.
	Scope      string            `json:"scope"`
	Custom     map[string]string `json:"custom"`
	Attributes map[string]string `json:"attributes"`
}

// Writes is whether the backend would let them change anything here, so a page can leave out
// what they could not use.
func (c *Caller) Writes() bool {
	return c != nil && (c.Admin || c.Scope == "rw" || c.Scope == "wo")
}

// Allows checks one of the plugin's custom permissions.
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

// Name is how to address them on a page.
func (c *Caller) Name() string {
	if c == nil || c.Label == "" {
		return "there"
	}
	return c.Label
}

// Event is one event the plugin subscribed to. Each is delivered at least once, so handling one
// twice must do no harm.
type Event struct {
	ID      string          `json:"id"`
	Topic   string          `json:"topic"`
	Source  string          `json:"source"`
	At      time.Time       `json:"at"`
	Payload json.RawMessage `json:"payload"`
}

// Run is what a run was started with: a schedule's {"schedule": name}, or what it was queued with.
type Run struct {
	Task    *string         `json:"task"`
	Payload json.RawMessage `json:"payload"`
}

// Schedule is the name of the schedule that started this run, if one did.
func (r Run) Schedule() string {
	var started struct {
		Schedule string `json:"schedule"`
	}
	_ = json.Unmarshal(r.Payload, &started)
	return started.Schedule
}

// Verdict is the plugin's opinion of settings someone is about to save: a problem against a key
// is shown on that field, and Problem refuses the save as a whole.
type Verdict struct {
	Problems map[string]string `json:"problems,omitempty"`
	Problem  string            `json:"problem,omitempty"`
	Message  string            `json:"message,omitempty"`
}

type problem struct {
	Type   string `json:"type"`
	Title  string `json:"title"`
	Status int    `json:"status"`
	Detail string `json:"detail,omitempty"`
}

type registerRequest struct {
	Manifest     Manifest  `json:"manifest"`
	Address      string    `json:"address"`
	BinarySHA256 string    `json:"binary_sha256"`
	StartedAt    time.Time `json:"started_at"`
}

type registerResponse struct {
	Secret     string `json:"secret"`
	State      string `json:"state"`
	LivenessMS uint64 `json:"liveness_ms"`
	Instance   string `json:"instance"`
}

type liveness struct {
	ID       string  `json:"id"`
	State    string  `json:"state"`
	Error    *string `json:"error"`
	Instance string  `json:"instance,omitempty"`
}
