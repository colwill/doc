// Reconciling {{ scaffold.kind }}s: look at what was asked for, look at what the cluster has, and
// make the second match the first. Every reconcile is a span, so a slow one shows up in the
// platform's telemetry beside everything else.

import * as k8s from "@kubernetes/client-node";
import { trace } from "@opentelemetry/api";

import type { Config, Flags } from "./platform/index.js";
import { GROUP, KIND, PLURAL, VERSION, type {{ scaffold.kind }} } from "./types.js";

const tracer = trace.getTracer("{{ values.name }}");

export class Controller {
  private readonly core: k8s.CoreV1Api;
  private readonly custom: k8s.CustomObjectsApi;

  constructor(
    private readonly kube: k8s.KubeConfig,
    private readonly config: Config,
    private readonly flags: Flags,
  ) {
    this.core = kube.makeApiClient(k8s.CoreV1Api);
    this.custom = kube.makeApiClient(k8s.CustomObjectsApi);
  }

  /** Watches every {{ scaffold.kind }} and reconciles each change; restarts its watch when it ends. */
  async run(): Promise<void> {
    const watch = new k8s.Watch(this.kube);
    const path = `/apis/${GROUP}/${VERSION}/${PLURAL}`;
    console.log(`watching ${KIND}s in ${GROUP} as ${this.config.service}`);

    const start = async (): Promise<void> => {
      await watch.watch(
        path,
        {},
        (kind: string, wanted: {{ scaffold.kind }}) => {
          if (kind === "ADDED" || kind === "MODIFIED") void this.reconcile(wanted);
        },
        (problem) => {
          // A watch that ends is not a failure; it is started again.
          if (problem !== null && problem !== undefined) console.warn("the watch ended", problem);
          setTimeout(() => void start(), 2000);
        },
      );
    };
    await start();
  }

  /** Brings one {{ scaffold.kind }} to what it asks for. */
  async reconcile(wanted: {{ scaffold.kind }}): Promise<void> {
    await tracer.startActiveSpan("reconcile", async (span) => {
      const name = wanted.metadata.name;
      const namespace = wanted.metadata.namespace ?? "default";
      span.setAttribute("resource.name", name);
      span.setAttribute("resource.namespace", namespace);

      try {
        // Turned off in DOC, this controller watches without changing anything: a way to stop it
        // acting without stopping it running.
        if (!this.flags.boolean("reconcile", true)) {
          console.log("reconciling is turned off in DOC; nothing was changed");
          return;
        }

        const message =
          wanted.spec.message !== undefined && wanted.spec.message !== ""
            ? wanted.spec.message
            : this.flags.string("default-message", "Made by {{ values.name }}");

        const map: k8s.V1ConfigMap = {
          metadata: {
            name,
            namespace,
            // The owner reference is what makes this go when the resource does.
            ownerReferences: [
              {
                apiVersion: `${GROUP}/${VERSION}`,
                kind: KIND,
                name,
                uid: wanted.metadata.uid ?? "",
                controller: true,
              },
            ],
          },
          data: { message, size: String(wanted.spec.size) },
        };

        try {
          await this.core.replaceNamespacedConfigMap({ name, namespace, body: map });
        } catch {
          await this.core.createNamespacedConfigMap({ namespace, body: map });
        }

        await this.custom.patchNamespacedCustomObjectStatus({
          group: GROUP,
          version: VERSION,
          namespace,
          plural: PLURAL,
          name,
          body: {
            status: {
              ready: true,
              reason: "Everything asked for is in place",
              observedGeneration: wanted.metadata.generation ?? 0,
            },
          },
        });
        console.log(`${namespace}/${name} is in place`);
      } catch (problem) {
        console.error(`${namespace}/${name} could not be reconciled`, problem);
      } finally {
        span.end();
      }
    });
  }
}
