package main

import (
	"encoding/json"
	"errors"
	"log/slog"
	"net/http"
	"strings"

	"{{ scaffold.module }}/internal/doc"
)

// routes are the plugin's pages, under /ui/, which DOC shows at /p/<id>/, and its API, under
// /api/, which is /api/v1/plugins/<id>/api/ to everyone else. DOC has already checked that the
// caller may read the plugin, and for anything but a GET that they may write to it.
func routes() http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("GET /ui", home)
	mux.HandleFunc("GET /ui/{$}", home)
	mux.HandleFunc("GET /ui/items/new", newItem)
	mux.HandleFunc("POST /ui/items", addFromPage)
	mux.HandleFunc("GET /api/items", listItems)
	mux.HandleFunc("POST /api/items", addFromAPI)
	return mux
}

// Home is what the page shows.
type Home struct {
	ID       string
	Title    string
	Greeting string
	Name     string
	Writes   bool
	Search   string
	Said     string
	Problem  string
	Items    []doc.Record
}

func home(w http.ResponseWriter, r *http.Request) {
	show(w, r, "", "")
}

// show draws the page, with what the last action said or what went wrong.
func show(w http.ResponseWriter, r *http.Request, said, problem string) {
	ctx, b := r.Context(), doc.From(r)
	view := Home{
		ID:      b.ID(),
		Title:   manifest().Nav[0].Label,
		Name:    b.Caller().Name(),
		Writes:  b.Caller().Writes(),
		Search:  r.URL.Query().Get("q"),
		Said:    said,
		Problem: problem,
	}
	settings, err := b.Settings(ctx)
	if err != nil {
		view.Problem = err.Error()
	}
	view.Greeting = settings.Text(settingGreeting)
	view.Items, _, err = b.Find(ctx, doc.Query{Collection: "items", Search: view.Search, Order: orderFor(view.Search), Limit: 50})
	if err != nil && view.Problem == "" {
		view.Problem = err.Error()
	}
	w.Header().Set("content-type", "text/html; charset=utf-8")
	if err := page.ExecuteTemplate(w, "home.html", view); err != nil {
		slog.Error("the page could not be drawn", "error", err)
	}
}

// NewItem is the page an item is added on. Adding is a button on the list that opens a page of its
// own, never a form under the list, and the button is there only for someone who may add.
type NewItem struct {
	ID      string
	Name    string
	Problem string
}

func newItem(w http.ResponseWriter, r *http.Request) {
	if !doc.From(r).Caller().Writes() {
		http.Error(w, "adding an item needs write access", http.StatusForbidden)
		return
	}
	showNew(w, r, "", "")
}

// showNew draws the form, with what was typed and why it was refused.
func showNew(w http.ResponseWriter, r *http.Request, name, problem string) {
	view := NewItem{ID: doc.From(r).ID(), Name: name, Problem: problem}
	w.Header().Set("content-type", "text/html; charset=utf-8")
	if err := page.ExecuteTemplate(w, "new_item.html", view); err != nil {
		slog.Error("the page could not be drawn", "error", err)
	}
}

// orderFor is newest first, unless a search orders them by how well they match.
func orderFor(search string) []doc.Order {
	if search != "" {
		return nil
	}
	return doc.Newest
}

// add keeps one item, as whoever asked.
func add(r *http.Request, name string) (doc.Record, error) {
	b := doc.From(r)
	return b.Insert(r.Context(), "items", map[string]any{"name": strings.TrimSpace(name), "by": b.Caller().Name()})
}

// addFromPage keeps the item and goes back to the list, or shows the form again with why not.
func addFromPage(w http.ResponseWriter, r *http.Request) {
	if err := r.ParseForm(); err != nil {
		showNew(w, r, "", err.Error())
		return
	}
	name := r.PostForm.Get("name")
	if strings.TrimSpace(name) == "" {
		showNew(w, r, name, "Give the item a name.")
		return
	}
	if _, err := add(r, name); err != nil {
		showNew(w, r, name, err.Error())
		return
	}
	// The list is drawn in the form's place, so the address bar goes back to it too.
	w.Header().Set("hx-push-url", "/p/"+doc.From(r).ID()+"/")
	show(w, r, "Added "+strings.TrimSpace(name)+".", "")
}

func listItems(w http.ResponseWriter, r *http.Request) {
	search := r.URL.Query().Get("q")
	items, next, err := doc.From(r).Find(r.Context(), doc.Query{
		Collection: "items",
		Search:     search,
		Order:      orderFor(search),
		Limit:      100,
		After:      r.URL.Query().Get("after"),
	})
	if err != nil {
		doc.Problem(w, http.StatusServiceUnavailable, "unavailable", err.Error())
		return
	}
	doc.JSON(w, http.StatusOK, map[string]any{"items": items, "next": next})
}

func addFromAPI(w http.ResponseWriter, r *http.Request) {
	var asked struct {
		Name string `json:"name"`
	}
	if err := json.NewDecoder(r.Body).Decode(&asked); err != nil || strings.TrimSpace(asked.Name) == "" {
		doc.Problem(w, http.StatusBadRequest, "bad-request", `send {"name": "…"}`)
		return
	}
	item, err := add(r, asked.Name)
	var refused *doc.Refused
	switch {
	case errors.As(err, &refused):
		doc.Problem(w, refused.Status, "refused", refused.Detail())
	case err != nil:
		doc.Problem(w, http.StatusServiceUnavailable, "unavailable", err.Error())
	default:
		doc.JSON(w, http.StatusCreated, item)
	}
}
