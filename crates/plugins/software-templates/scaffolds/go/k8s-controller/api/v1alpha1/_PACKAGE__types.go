package v1alpha1

import (
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
)

// {{ scaffold.kind }}Spec is what somebody asks for.
type {{ scaffold.kind }}Spec struct {
	// Size is how many of them to run.
	// +kubebuilder:validation:Minimum=0
	// +kubebuilder:default=1
	Size int32 `json:"size"`

	// Message is what they say, which is what this controller writes into their config map.
	// +kubebuilder:validation:MaxLength=256
	// +optional
	Message string `json:"message,omitempty"`
}

// {{ scaffold.kind }}Status is what the cluster is actually doing about it.
type {{ scaffold.kind }}Status struct {
	// Conditions follow the usual Kubernetes conventions: Ready is the one to watch.
	// +optional
	Conditions []metav1.Condition `json:"conditions,omitempty"`

	// Observed is the generation this status was worked out from.
	// +optional
	Observed int64 `json:"observedGeneration,omitempty"`
}

// +kubebuilder:object:root=true
// +kubebuilder:subresource:status
// +kubebuilder:printcolumn:name="Size",type=integer,JSONPath=`.spec.size`
// +kubebuilder:printcolumn:name="Ready",type=string,JSONPath=`.status.conditions[?(@.type=="Ready")].status`
// +kubebuilder:printcolumn:name="Age",type=date,JSONPath=`.metadata.creationTimestamp`

// {{ scaffold.kind }} is what {{ values.name }} reconciles.
type {{ scaffold.kind }} struct {
	metav1.TypeMeta   `json:",inline"`
	metav1.ObjectMeta `json:"metadata,omitempty"`

	Spec   {{ scaffold.kind }}Spec   `json:"spec,omitempty"`
	Status {{ scaffold.kind }}Status `json:"status,omitempty"`
}

// +kubebuilder:object:root=true

// {{ scaffold.kind }}List is a list of them.
type {{ scaffold.kind }}List struct {
	metav1.TypeMeta `json:",inline"`
	metav1.ListMeta `json:"metadata,omitempty"`
	Items           []{{ scaffold.kind }} `json:"items"`
}

func init() {
	SchemeBuilder.Register(&{{ scaffold.kind }}{}, &{{ scaffold.kind }}List{})
}
