#!/usr/bin/env python3
"""Checks the manifests in this directory without talking to a cluster.

A schema check tells you a field is spelled right. These are the things that are spelled right and
still do not work: a Service whose selector matches no pod, a volume mount with no volume behind
it, a claim or ConfigMap or Secret nothing declares, a namespace nobody creates. All of it comes
from reading the YAML, so it needs no kubeconfig, no cluster and no `kubectl`.

    python3 deploy/kubernetes/validate.py
"""

import collections
import pathlib
import sys

import yaml

HERE = pathlib.Path(__file__).parent
WORKLOADS = ("Deployment", "StatefulSet", "Job")


def load():
    """Every document in every file, with the file it came from."""
    found = []
    for path in sorted(HERE.rglob("*.yaml")):
        try:
            for document in yaml.safe_load_all(path.read_text()):
                if document:
                    found.append((path.relative_to(HERE), document))
        except yaml.YAMLError as err:
            sys.exit(f"{path}: {err}")
    return found


def pod_spec(document):
    return document["spec"]["template"]["spec"]


def check(documents):
    problems = []
    declared = collections.defaultdict(set)
    for _, document in documents:
        declared[document["kind"]].add(
            (document["metadata"].get("namespace"), document["metadata"]["name"])
        )

    workloads = {}
    for path, document in documents:
        kind = document.get("kind")
        name = document.get("metadata", {}).get("name")
        for key in ("apiVersion", "kind"):
            if not document.get(key):
                problems.append(f"{path}: a document has no {key}")
        if not name:
            problems.append(f"{path}: a {kind} has no metadata.name")
        namespace = document.get("metadata", {}).get("namespace")
        if namespace and (None, namespace) not in declared["Namespace"]:
            problems.append(f"{path}: namespace '{namespace}' is never declared")

        if kind in ("Deployment", "StatefulSet"):
            labels = document["spec"]["template"]["metadata"]["labels"]
            workloads[(namespace, name)] = labels
            selector = document["spec"]["selector"]["matchLabels"]
            if not all(labels.get(key) == value for key, value in selector.items()):
                problems.append(f"{path}: {name}'s selector does not match its own pod labels")

        if kind in WORKLOADS:
            spec = pod_spec(document)
            have = {volume["name"] for volume in spec.get("volumes", [])}
            have |= {
                claim["metadata"]["name"]
                for claim in document["spec"].get("volumeClaimTemplates", [])
            }
            for container in spec["containers"]:
                for mount in container.get("volumeMounts", []):
                    if mount["name"] not in have:
                        problems.append(
                            f"{path}: {name} mounts '{mount['name']}', which it has no volume for"
                        )
                for source in container.get("envFrom", []):
                    if "secretRef" in source:
                        wanted = source["secretRef"]["name"]
                        if (namespace, wanted) not in declared["Secret"]:
                            problems.append(
                                f"{path}: secret '{wanted}' is not declared in {namespace}"
                            )
            for volume in spec.get("volumes", []):
                if "persistentVolumeClaim" in volume:
                    wanted = volume["persistentVolumeClaim"]["claimName"]
                    if (namespace, wanted) not in declared["PersistentVolumeClaim"]:
                        problems.append(f"{path}: claim '{wanted}' is not declared in {namespace}")
                if "configMap" in volume:
                    wanted = volume["configMap"]["name"]
                    if (namespace, wanted) not in declared["ConfigMap"]:
                        problems.append(
                            f"{path}: configMap '{wanted}' is not declared in {namespace}"
                        )

    # A Service that fronts nothing is the failure that looks like everything is fine.
    for path, document in documents:
        if document.get("kind") != "Service":
            continue
        selector = document["spec"].get("selector")
        if not selector:
            continue
        namespace = document["metadata"].get("namespace")
        fronts = [
            name
            for (workload_namespace, name), labels in workloads.items()
            if workload_namespace == namespace
            and all(labels.get(key) == value for key, value in selector.items())
        ]
        if not fronts:
            problems.append(
                f"{path}: Service {document['metadata']['name']} selects nothing in {namespace}"
            )
    return problems


documents = load()
problems = check(documents)
print(f"{len(documents)} documents read from {HERE}")
if problems:
    for problem in problems:
        print(f"  - {problem}")
    sys.exit(f"{len(problems)} problems")
print("no problems")
