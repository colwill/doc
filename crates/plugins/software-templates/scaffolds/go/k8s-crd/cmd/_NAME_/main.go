// Command {{ values.name }} works with {{ scaffold.kind }}s from the command line: it lists them,
// and it writes one. It is here so the generated types are used by something from the start.
package main

import (
	"context"
	"flag"
	"fmt"
	"log/slog"
	"os"

	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/client/config"

	"{{ scaffold.module }}/api/v1alpha1"
	"{{ scaffold.module }}/internal/platform"
)

func main() {
	namespace := flag.String("namespace", "default", "the namespace to work in")
	create := flag.String("create", "", "the name of a {{ scaffold.kind }} to create")
	owner := flag.String("owner", "{{ values.team | name | default('platform') }}", "the team answering for it")
	flag.Parse()

	ctx := context.Background()
	slog.SetDefault(slog.New(slog.NewJSONHandler(os.Stderr, nil)))
	settings := platform.Load()
	telemetry, err := platform.StartTelemetry(ctx, settings)
	if err == nil {
		defer func() { _ = telemetry.Shutdown(ctx) }()
	}

	scheme := runtime.NewScheme()
	if err := v1alpha1.AddToScheme(scheme); err != nil {
		fail("the scheme could not be built", err)
	}
	cluster, err := client.New(config.GetConfigOrDie(), client.Options{Scheme: scheme})
	if err != nil {
		fail("the cluster could not be reached", err)
	}

	if *create != "" {
		wanted := &v1alpha1.{{ scaffold.kind }}{
			ObjectMeta: metav1.ObjectMeta{Name: *create, Namespace: *namespace},
			Spec:       v1alpha1.{{ scaffold.kind }}Spec{Owner: *owner, Size: 1},
		}
		if err := cluster.Create(ctx, wanted); err != nil {
			fail("it could not be created", err)
		}
		fmt.Printf("{{ scaffold.kind }}/%s created in %s\n", *create, *namespace)
		return
	}

	var held v1alpha1.{{ scaffold.kind }}List
	if err := cluster.List(ctx, &held, client.InNamespace(*namespace)); err != nil {
		fail("they could not be listed", err)
	}
	if len(held.Items) == 0 {
		fmt.Printf("there are no {{ scaffold.plural }} in %s\n", *namespace)
		return
	}
	for _, one := range held.Items {
		fmt.Printf("%-30s owner=%-20s size=%d phase=%s\n", one.Name, one.Spec.Owner, one.Spec.Size, one.Status.Phase)
	}
}

func fail(what string, err error) {
	slog.Error(what, "error", err)
	os.Exit(1)
}
