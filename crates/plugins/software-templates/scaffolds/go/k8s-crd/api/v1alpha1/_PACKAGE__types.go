package v1alpha1

import (
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
)

// Phase is where a {{ scaffold.kind }} has got to.
// +kubebuilder:validation:Enum=Pending;Ready;Failed
type Phase string

const (
	Pending Phase = "Pending"
	Ready   Phase = "Ready"
	Failed  Phase = "Failed"
)

// {{ scaffold.kind }}Spec is what somebody asks for. Everything a cluster will be held to belongs
// here, with the validation that makes a wrong one impossible rather than merely reported.
type {{ scaffold.kind }}Spec struct {
	// Owner is the team answering for it, as the Catalogue in DOC names them.
	// +kubebuilder:validation:Pattern=`^[a-z][a-z0-9-]{1,62}$`
	Owner string `json:"owner"`

	// Size is how many of them to run.
	// +kubebuilder:validation:Minimum=0
	// +kubebuilder:validation:Maximum=100
	// +kubebuilder:default=1
	Size int32 `json:"size"`

	// Retention is how long what it makes is kept.
	// +kubebuilder:validation:Pattern=`^[0-9]+(h|m|s)$`
	// +kubebuilder:default="168h"
	// +optional
	Retention string `json:"retention,omitempty"`

	// Settings are passed through to whatever runs it.
	// +optional
	Settings map[string]string `json:"settings,omitempty"`
}

// {{ scaffold.kind }}Status is what is actually true of it.
type {{ scaffold.kind }}Status struct {
	// +optional
	Phase Phase `json:"phase,omitempty"`
	// +optional
	Conditions []metav1.Condition `json:"conditions,omitempty"`
	// +optional
	Observed int64 `json:"observedGeneration,omitempty"`
}

// +kubebuilder:object:root=true
// +kubebuilder:subresource:status
// +kubebuilder:resource:shortName={{ scaffold.kind | lower }}
// +kubebuilder:printcolumn:name="Owner",type=string,JSONPath=`.spec.owner`
// +kubebuilder:printcolumn:name="Size",type=integer,JSONPath=`.spec.size`
// +kubebuilder:printcolumn:name="Phase",type=string,JSONPath=`.status.phase`
// +kubebuilder:printcolumn:name="Age",type=date,JSONPath=`.metadata.creationTimestamp`

// {{ scaffold.kind }} is {{ values.description }}
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
