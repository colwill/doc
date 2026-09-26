# Running DOC on Kubernetes

Two namespaces. **`doc-system`** is the platform — Postgres, the three buses, the backend, the
frontend and the workers. **`doc-plugins`** holds one Service and one Deployment per plugin, each
connecting to `backend.doc-system.svc.cluster.local:4433`.

Keeping them apart is what lets a plugin have its own quota, its own network policy and its own
service account without any of that reaching the database or the buses.

```
doc-system/
  00-namespace.yaml   Both namespaces
  10-config.yaml      config/doc.toml and the database's init scripts — generated, see below
  20-storage.yaml     Postgres, and the volume everything reads its certificates from
  30-fabric.yaml      Three nodes each of the event, service and cache buses, and telemetry
  40-bootstrap.yaml   The Job that fills the secrets volume
  50-core.yaml        The backend, the frontend and the way in
  60-workers.yaml     What runs queued work and schedules
plugins/
  00-secrets-claim.yaml
  <id>.yaml           One Service and one Deployment each, 38 of them; dns.yaml also has
                      dns-server, the DNS port people ask
```

## Apply it

```sh
# The configuration comes from the files the repository already keeps, rather than a second copy.
./generate-config.sh

kubectl apply -f doc-system/00-namespace.yaml
kubectl apply -f doc-system/

# Wait for the secrets to be written before anything tries to register.
kubectl -n doc-system wait --for=condition=complete job/doc-bootstrap --timeout=5m

kubectl apply -f plugins/
```

## What has to be filled in first

**The images.** Every manifest says `doc/<name>:dev`, which no registry serves. Build and push
them, then set the registry — one `kustomize edit set image`, or a `sed`:

```sh
sed -i 's|image: doc/|image: registry.example.com/doc/|' doc-system/*.yaml plugins/*.yaml
```

**The secrets volume.** `doc-secrets` is `ReadWriteMany` because the bootstrap Job writes it once
and the backend, the frontend, the workers, all nine bus nodes and 38 plugins then read it at the
same time, across nodes. A cluster whose storage class cannot do RWX has two ways out:

- give it one that can — NFS, CephFS, Azure Files, EFS; or
- run the bootstrap somewhere else, then load the directory as a Secret and mount that instead:
  `kubectl -n doc-system create secret generic doc-secrets --from-file=<the directory>`, changing
  each `persistentVolumeClaim` to a `secret` in the manifests. A Secret is capped at 1 MiB, which
  the bootstrap output fits inside comfortably.

The claim in `doc-plugins` binds the same volume. A PersistentVolumeClaim does not cross
namespaces, so fill in its `volumeName` with what the first one bound to:

```sh
kubectl -n doc-system get pvc doc-secrets -o jsonpath='{.spec.volumeName}'
```

**The passwords.** `postgres-credentials` and `doc-database` both say `dev-only-change-me`. They
are plain `stringData` so they can be read; put real ones in whatever this cluster uses for
secrets — Sealed Secrets, External Secrets, SOPS — and delete them from here.

**The ingress.** `rundoc.sh`, with a wildcard rule beside it. The wildcard is what makes a
plugin's own name work (`rbac.rundoc.sh` → `/p/rbac/`), so the certificate needs to cover it
too. Set `DOC_PUBLIC_URL` on the frontend to the same host, or the redirect will not happen.

## The DNS server

The `dns` plugin is the one plugin people reach directly rather than through the backend: a
resolver asks it on port 53. `plugins/dns.yaml` gives it a second Service for that, `dns-server`,
a LoadBalancer sending 53 over UDP and TCP to the 1053 the plugin listens on. To use it:

1. Turn on **DNS server** under the plugin's Features tab, and leave **Listen on** at port 1053,
   which is where the Service sends.
2. Read the address the load balancer was given (`kubectl -n doc-plugins get svc dns-server`) and
   fill in the plugin's settings:
   - **Forward everything else to** — without it, every name outside DOC's domains is refused.
     The cluster's own DNS Service (`kubectl -n kube-system get svc kube-dns`) or the company's
     resolvers, by address.
   - **Forward only for** — the networks developers ask from. The default is the private ranges;
     anyone else gets DOC's own domains and nothing more.
   - **Where the name servers are reached** — that address, so `ns1.rundoc.sh` answers with it.
     `DOC_DNS_NAMESERVER_ADDRESSES` on the Deployment does the same.
3. Give developers that address as their DNS server. A resolver is always set by address, so this
   is usually pushed rather than typed: by the VPN, DHCP, or device management.

**The domain.** DOC answers for the domain in `[instance] domain` in the ConfigMap's `doc.toml`
(`rundoc.sh` here) and names its plugins under it until its settings say otherwise, and each
domain is served by `ns1.<the domain>` unless **Name servers** says different. Another deployment
sets its own there, or with `DOC_INSTANCE_DOMAIN` on the backend; it should be the host in
`DOC_PUBLIC_URL`. `DOC_DNS_ZONES` and `DOC_DNS_PLUGIN_DOMAIN` still override it for DNS alone. A cloud gives a load balancer a new address each time it is made unless one
is reserved, so reserve one before writing it into DNS anywhere.

**Handing a domain to DOC** means NS records at its parent naming `ns1.<the domain>`, with a glue
record giving that name the load balancer's address. Resolvers then come to DOC for every name in
it without anybody changing their settings — but DOC is then the only server for that domain,
and it runs one replica that is replaced on every deploy. Most registries also ask for two name
servers. Handing it a subdomain, rather than the domain the website and mail live in, keeps a
restart from taking those down with it.

**Where there is no load balancer**, EXTERNAL-IP stays pending and the node running the pod answers
on NodePort 30053. Most operating systems only ask port 53, so this suits resolvers that take a
port: systemd-resolved (`DNS=10.0.0.5:30053`), `port 30053` in a macOS `/etc/resolver/<domain>`,
or a company resolver that forwards DOC's domains there.

**The asker's address has to arrive intact**, since that is what **Forward only for** decides by.
`externalTrafficPolicy: Local` keeps it. Without it every question arrives from a node, inside the
private ranges, and DOC forwards for the whole internet.

**DNS over HTTPS** needs none of this: the ingress sends the two `dns-query` paths under
`rundoc.sh` to the backend, so browsers and resolvers can be given
`https://rundoc.sh/api/v1/plugins/dns/api/dns-query` once **DNS over HTTPS** is on.

## Things worth knowing

**The buses are StatefulSets, not Deployments.** A node's identity and its data are its own:
`eventbus-0` keeps being node 1 across a restart, with the same volume and the same name for its
peers to dial. The node ID is the pod's ordinal plus one, worked out in the container, because the
buses count from one and Kubernetes from zero.

**A plugin runs one replica, and replaces rather than rolls.** A plugin registers as itself; a
second copy of the same version hands over to the first rather than sharing the work, so two
replicas would be one plugin and one spare that keeps trying. `strategy: Recreate` is what stops a
rolling update making that happen on every deploy.

**A plugin added to `config/doc.toml` needs the bootstrap Job run again** before it can register —
it has no token until then. Delete the completed Job and apply it again.

**QUIC is UDP.** Every Service here that carries it says so. An ingress controller or service mesh
that only understands TCP will not carry the plugin protocol, which is why plugins are reached
inside the cluster rather than through the ingress.

## What was checked

`python3 validate.py`, which reads every document here and checks the things that are spelled
right and still do not work: a Service whose selector matches no pod, a volume mount with no
volume behind it, a claim, ConfigMap or Secret nothing declares, a namespace nobody creates. It
needs no cluster and no `kubectl`. All 102 documents pass.

**These have not been deployed.** Doing that needs the 41 images built and pushed first. Treat the
manifests as a starting point that is structurally right, not as something known to run.
