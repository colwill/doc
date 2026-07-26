package main

import (
	"log/slog"
	"net/http"

	"{{ scaffold.module }}/internal/doc"
)

// routes are the plugin's pages, under /ui/, which DOC shows at /p/<id>/, and its API, under
// /api/, which is /api/v1/plugins/<id>/api/ to everyone else. DOC has already checked that the
// caller may read the plugin, and for a POST that they may write to it.
func routes() http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("GET /ui", home)
	mux.HandleFunc("GET /ui/{$}", home)
	mux.HandleFunc("GET /ui/cheers", cheersTable)
	mux.HandleFunc("POST /ui/cheers", cheerFromPage)
	mux.HandleFunc("GET /api/cheers", listCheers)
	mux.HandleFunc("POST /api/cheers", cheerFromAPI)
	return mux
}

// Home is what the page shows.
type Home struct {
	ID      string
	Title   string
	Name    string
	Person  string
	Place   string
	Sky     string
	Cloudy  bool
	Writes  bool
	Said    string
	Problem string
	Cheers  []doc.Record
}

func home(w http.ResponseWriter, r *http.Request) {
	show(w, r, "", "")
}

// show draws the whole page, with what the last action said or what went wrong.
func show(w http.ResponseWriter, r *http.Request, said, problem string) {
	ctx, b := r.Context(), doc.From(r)
	view := Home{
		ID:      b.ID(),
		Title:   manifest().Nav[0].Label,
		Name:    b.Caller().Name(),
		Writes:  b.Caller().Writes(),
		Said:    said,
		Problem: problem,
	}
	settings, err := b.Settings(ctx)
	if err != nil {
		view.Problem = err.Error()
	} else {
		view.Person, view.Place = settings.Text(settingPerson), settings.Text(settingPlace)
		sky, err := lookUp(ctx, settings.Number(settingLatitude, 51.5072), settings.Number(settingLongitude, -0.1276))
		if err != nil && view.Problem == "" {
			view.Problem = err.Error()
		}
		if err == nil {
			view.Sky, view.Cloudy = sky.String(), sky.Cloudy(settings.Number(settingCloudyAt, 70))
		}
	}
	view.Cheers, _, err = b.Find(ctx, doc.Query{Collection: "cheers", Order: doc.Newest, Limit: 20})
	if err != nil && view.Problem == "" {
		view.Problem = err.Error()
	}
	w.Header().Set("content-type", "text/html; charset=utf-8")
	if err := page.ExecuteTemplate(w, "home.html", view); err != nil {
		slog.Error("the page could not be drawn", "error", err)
	}
}

// cheersTable is the table on its own, which the page loads again when a cheer is sent.
func cheersTable(w http.ResponseWriter, r *http.Request) {
	b := doc.From(r)
	view := Home{ID: b.ID()}
	view.Cheers, _, _ = b.Find(r.Context(), doc.Query{Collection: "cheers", Order: doc.Newest, Limit: 20})
	w.Header().Set("content-type", "text/html; charset=utf-8")
	if err := page.ExecuteTemplate(w, "cheers", view); err != nil {
		slog.Error("the table could not be drawn", "error", err)
	}
}

// cheerFromPage is the button: the page is drawn again with what happened.
func cheerFromPage(w http.ResponseWriter, r *http.Request) {
	outcome, err := cheer(r.Context(), doc.From(r), doc.From(r).Caller().Name(), false)
	if err != nil {
		show(w, r, "", err.Error())
		return
	}
	show(w, r, outcome.Said, "")
}

func listCheers(w http.ResponseWriter, r *http.Request) {
	cheers, next, err := doc.From(r).Find(r.Context(), doc.Query{Collection: "cheers", Order: doc.Newest, Limit: 50, After: r.URL.Query().Get("after")})
	if err != nil {
		doc.Problem(w, http.StatusServiceUnavailable, "unavailable", err.Error())
		return
	}
	doc.JSON(w, http.StatusOK, map[string]any{"cheers": cheers, "next": next})
}

// cheerFromAPI is the operation automations call by name.
func cheerFromAPI(w http.ResponseWriter, r *http.Request) {
	outcome, err := cheer(r.Context(), doc.From(r), doc.From(r).Caller().Name(), false)
	if err != nil {
		doc.Problem(w, http.StatusUnprocessableEntity, "not-cheered", err.Error())
		return
	}
	doc.JSON(w, http.StatusOK, outcome)
}
