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
	return mux
}

// Home is what the page shows.
type Home struct {
	Title string
	Name  string
}

func home(w http.ResponseWriter, r *http.Request) {
	b := doc.From(r)
	view := Home{Title: manifest().Nav[0].Label, Name: b.Caller().Name()}
	w.Header().Set("content-type", "text/html; charset=utf-8")
	if err := page.ExecuteTemplate(w, "home.html", view); err != nil {
		slog.Error("the page could not be drawn", "error", err)
	}
}
