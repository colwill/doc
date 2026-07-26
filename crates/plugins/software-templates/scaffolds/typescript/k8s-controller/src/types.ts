// The custom resource this controller reconciles, as TypeScript sees it. The CRD in
// config/crd/bases is the schema the cluster enforces; this is the same thing for the code.

export const GROUP = "{{ scaffold.group }}";
export const VERSION = "v1alpha1";
export const PLURAL = "{{ scaffold.plural }}";
export const KIND = "{{ scaffold.kind }}";

export interface Spec {
  /** How many of them to run. */
  size: number;
  /** What they say. DOC's `default-message` flag is used when this is empty. */
  message?: string;
}

export interface Status {
  ready?: boolean;
  reason?: string;
  observedGeneration?: number;
}

export interface {{ scaffold.kind }} {
  apiVersion?: string;
  kind?: string;
  metadata: {
    name: string;
    namespace?: string;
    uid?: string;
    generation?: number;
    resourceVersion?: string;
  };
  spec: Spec;
  status?: Status;
}
