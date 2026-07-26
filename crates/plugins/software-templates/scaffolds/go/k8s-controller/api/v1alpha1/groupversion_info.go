// Package v1alpha1 holds the API types {{ values.name }} reconciles.
// +kubebuilder:object:generate=true
// +groupName={{ scaffold.group }}
package v1alpha1

import (
	"k8s.io/apimachinery/pkg/runtime/schema"
	"sigs.k8s.io/controller-runtime/pkg/scheme"
)

var (
	// GroupVersion is the group and version of every type in this package.
	GroupVersion = schema.GroupVersion{Group: "{{ scaffold.group }}", Version: "v1alpha1"}

	// SchemeBuilder registers them with a scheme.
	SchemeBuilder = &scheme.Builder{GroupVersion: GroupVersion}

	// AddToScheme adds them to a scheme.
	AddToScheme = SchemeBuilder.AddToScheme
)
