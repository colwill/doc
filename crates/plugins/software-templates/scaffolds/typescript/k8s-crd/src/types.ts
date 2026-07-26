// The custom resource, as TypeScript sees it. The CRD in config/crd/bases is the schema the cluster
// enforces; the zod schema below is the same rules again, so a wrong resource is refused here
// before the API server has to refuse it, and `npm run schema` prints what it comes to.

import { z } from "zod";

export const GROUP = "{{ scaffold.group }}";
export const VERSION = "v1alpha1";
export const PLURAL = "{{ scaffold.plural }}";
export const KIND = "{{ scaffold.kind }}";

export const Phase = z.enum(["Pending", "Ready", "Failed"]);
export type Phase = z.infer<typeof Phase>;

/** What somebody asks for. */
export const Spec = z.object({
  /** The team answering for it, as the Catalogue in DOC names them. */
  owner: z.string().regex(/^[a-z][a-z0-9-]{1,62}$/),
  /** How many of them to run. */
  size: z.number().int().min(0).max(100).default(1),
  /** How long what it makes is kept. */
  retention: z.string().regex(/^[0-9]+(h|m|s)$/).default("168h"),
  /** Passed through to whatever runs it. */
  settings: z.record(z.string()).default({}),
});
export type Spec = z.infer<typeof Spec>;

/** What is actually true of it. */
export const Status = z.object({
  phase: Phase.default("Pending"),
  reason: z.string().optional(),
  observedGeneration: z.number().int().default(0),
});
export type Status = z.infer<typeof Status>;

/** A whole resource, as the API server returns it. */
export const Resource = z.object({
  apiVersion: z.string().default(`${GROUP}/${VERSION}`),
  kind: z.string().default(KIND),
  metadata: z.object({
    name: z.string(),
    namespace: z.string().default("default"),
    uid: z.string().optional(),
    generation: z.number().optional(),
  }),
  spec: Spec,
  status: Status.optional(),
});
export type Resource = z.infer<typeof Resource>;
