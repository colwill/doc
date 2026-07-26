package main

import (
	"context"
	"fmt"

	"{{ scaffold.module }}/internal/doc"
)

// Person is somebody in DOC.
type Person struct {
	ID    string
	Login string
}

// findPerson looks somebody up by their login. Every plugin may read core.users; none may change it.
func findPerson(ctx context.Context, b *doc.Backend, login string) (Person, error) {
	found, _, err := b.Find(ctx, doc.Query{
		Collection: "core.users",
		Where:      map[string]any{"login": login},
		Limit:      1,
	})
	if err != nil {
		return Person{}, fmt.Errorf("DOC could not be asked who %s is: %w", login, err)
	}
	if len(found) == 0 {
		return Person{}, fmt.Errorf("nobody in DOC signs in as %s", login)
	}
	return Person{ID: found[0].Text("id"), Login: login}, nil
}

// tell puts a notification in their DOC inbox: the bell in the header, and /p/notifications/.
func tell(ctx context.Context, b *doc.Backend, person Person, title, message, link string) error {
	return b.Notify(ctx, person.ID, title, message, link)
}
