#!/usr/bin/env node
// {{ values.name }} — {{ values.description }}
//
// The custom resource lives in types.ts; this reads and writes them, so the types are used by
// something from the first commit.
//
//   {{ values.name }} list
//   {{ values.name }} create my-thing

import * as k8s from "@kubernetes/client-node";

import { loadConfig, Flags, startTelemetry } from "./platform/index.js";
import { GROUP, PLURAL, VERSION, Resource, Spec } from "./types.js";

const [command = "list", argument] = process.argv.slice(2);
const config = loadConfig();
const shutdown = startTelemetry(config);
const flags = await Flags.start(config);

const kube = new k8s.KubeConfig();
kube.loadFromDefault();
const custom = kube.makeApiClient(k8s.CustomObjectsApi);
const namespace = process.env.NAMESPACE ?? "default";

try {
  if (command === "create") {
    const name = argument ?? "{{ values.name }}-sample";
    const spec = Spec.parse({
      owner: flags.string("default-owner", "{{ values.team | name | default('platform') }}"),
      size: 1,
    });
    const made = await custom.createNamespacedCustomObject({
      group: GROUP,
      version: VERSION,
      namespace,
      plural: PLURAL,
      body: { apiVersion: `${GROUP}/${VERSION}`, kind: "{{ scaffold.kind }}", metadata: { name }, spec },
    });
    const resource = Resource.parse(made);
    console.log(`{{ scaffold.kind }}/${resource.metadata.name} created in ${namespace}`);
  } else {
    const held = await custom.listNamespacedCustomObject({
      group: GROUP,
      version: VERSION,
      namespace,
      plural: PLURAL,
    });
    const items = (held as { items?: unknown[] }).items ?? [];
    if (items.length === 0) console.log(`there are no {{ scaffold.plural }} in ${namespace}`);
    for (const one of items) {
      const resource = Resource.parse(one);
      const phase = resource.status?.phase ?? "Pending";
      console.log(
        `${resource.metadata.name.padEnd(30)} owner=${resource.spec.owner.padEnd(20)} size=${resource.spec.size} phase=${phase}`,
      );
    }
  }
} finally {
  flags.stop();
  await shutdown();
}
