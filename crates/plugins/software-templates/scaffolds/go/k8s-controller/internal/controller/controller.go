// Package controller reconciles {{ scaffold.kind }}s: it looks at what somebody asked for, looks at
// what the cluster has, and makes the second match the first. Every reconcile is a span, so a slow
// one shows up in the platform's telemetry beside everything else.
package controller

import (
	"context"
	"fmt"
	"time"

	corev1 "k8s.io/api/core/v1"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/controller/controllerutil"
	"sigs.k8s.io/controller-runtime/pkg/log"

	"go.opentelemetry.io/otel"
	"go.opentelemetry.io/otel/attribute"

	"{{ scaffold.module }}/api/v1alpha1"
	"{{ scaffold.module }}/internal/platform"
)

// Reconciler brings each {{ scaffold.kind }} to what it asks for.
type Reconciler struct {
	client.Client
	Scheme *runtime.Scheme
	Config platform.Config
	Flags  *platform.Flags
}

// +kubebuilder:rbac:groups={{ scaffold.group }},resources={{ scaffold.plural }},verbs=get;list;watch;create;update;patch;delete
// +kubebuilder:rbac:groups={{ scaffold.group }},resources={{ scaffold.plural }}/status,verbs=get;update;patch
// +kubebuilder:rbac:groups="",resources=configmaps,verbs=get;list;watch;create;update;patch;delete

// Reconcile is called for every change to a {{ scaffold.kind }}, and again on a schedule.
func (r *Reconciler) Reconcile(ctx context.Context, request ctrl.Request) (ctrl.Result, error) {
	ctx, span := otel.Tracer(r.Config.Service).Start(ctx, "reconcile")
	defer span.End()
	span.SetAttributes(
		attribute.String("resource.name", request.Name),
		attribute.String("resource.namespace", request.Namespace),
	)
	logger := log.FromContext(ctx)

	// Turned off in DOC, this controller watches without changing anything: a way to stop it
	// acting without stopping it running.
	if !r.Flags.Bool("reconcile", true) {
		logger.Info("reconciling is turned off in DOC; nothing was changed")
		return ctrl.Result{RequeueAfter: time.Minute}, nil
	}

	var wanted v1alpha1.{{ scaffold.kind }}
	if err := r.Get(ctx, request.NamespacedName, &wanted); err != nil {
		// Gone: whatever it owned goes with it, because the owner reference says so.
		return ctrl.Result{}, client.IgnoreNotFound(err)
	}

	config := &corev1.ConfigMap{
		ObjectMeta: metav1.ObjectMeta{Name: wanted.Name, Namespace: wanted.Namespace},
	}
	outcome, err := controllerutil.CreateOrUpdate(ctx, r.Client, config, func() error {
		config.Data = map[string]string{
			"message": message(&wanted, r.Flags),
			"size":    fmt.Sprintf("%d", wanted.Spec.Size),
		}
		return controllerutil.SetControllerReference(&wanted, config, r.Scheme)
	})
	if err != nil {
		r.setReady(ctx, &wanted, metav1.ConditionFalse, "ConfigMapFailed", err.Error())
		return ctrl.Result{}, err
	}
	if outcome != controllerutil.OperationResultNone {
		logger.Info("the config map was brought up to date", "outcome", outcome)
	}

	r.setReady(ctx, &wanted, metav1.ConditionTrue, "Reconciled", "Everything asked for is in place")
	return ctrl.Result{RequeueAfter: 5 * time.Minute}, nil
}

// message is what the resource asks for, or what DOC says every one of them should say.
func message(wanted *v1alpha1.{{ scaffold.kind }}, flags *platform.Flags) string {
	if wanted.Spec.Message != "" {
		return wanted.Spec.Message
	}
	return flags.String("default-message", "Made by {{ values.name }}")
}

func (r *Reconciler) setReady(ctx context.Context, wanted *v1alpha1.{{ scaffold.kind }}, status metav1.ConditionStatus, reason, detail string) {
	meta := metav1.Condition{
		Type:               "Ready",
		Status:             status,
		Reason:             reason,
		Message:            detail,
		ObservedGeneration: wanted.Generation,
		LastTransitionTime: metav1.Now(),
	}
	changed := true
	for at, held := range wanted.Status.Conditions {
		if held.Type != "Ready" {
			continue
		}
		changed = held.Status != status || held.Reason != reason
		wanted.Status.Conditions[at] = meta
		break
	}
	if changed && len(wanted.Status.Conditions) == 0 {
		wanted.Status.Conditions = append(wanted.Status.Conditions, meta)
	}
	wanted.Status.Observed = wanted.Generation
	if err := r.Status().Update(ctx, wanted); err != nil && !apierrors.IsConflict(err) {
		log.FromContext(ctx).Error(err, "the status could not be written")
	}
}

// SetupWithManager tells the manager what to watch.
func (r *Reconciler) SetupWithManager(manager ctrl.Manager) error {
	return ctrl.NewControllerManagedBy(manager).
		For(&v1alpha1.{{ scaffold.kind }}{}).
		Owns(&corev1.ConfigMap{}).
		Named("{{ values.name }}").
		Complete(r)
}
