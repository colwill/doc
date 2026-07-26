//! The templates the plugin starts with, so a platform has something to create from on the day it
//! is installed. They are ordinary templates: an author can change one, and removing one leaves it
//! gone until the plugin is loaded again with nothing of that name there.

use doc_plugin_sdk::Backend;

use crate::Refusal;
use crate::model;
use crate::store::Store;

/// The whole path: a repository scaffolded in one of five languages as one of five kinds of
/// application, wired to the platform's telemetry and its feature flags, in the Catalogue, and
/// with its first flags already in DOC.
const SERVICE: &str = r#"
name: new-service
title: New service
description: >-
  Creates a repository from one of the platform's scaffolds - Go, Rust, Python, C++ or TypeScript,
  as a CLI, a REST or gRPC API, a Kubernetes controller or a set of CRDs - already exporting to this
  platform's telemetry stack and reading its feature flags from DOC. It is put in the Catalogue with
  its repository and how far along it is, and given the flags it starts with. It can be a download
  instead of a repository, which needs no git host.
kind: Service
tags: [service, scaffold, github]
parameters:
  - name: name
    title: Service name
    hint: Lowercase letters, digits and dashes. It names the repository, the binary and the package.
    required: true
    pattern: "^[a-z][a-z0-9-]{1,38}$"
    expects: A service name is 2 to 39 lowercase letters, digits and dashes, starting with a letter.
    placeholder: payments-api
  - name: description
    kind: textarea
    title: What it does
    required: true
    hint: A sentence or two. It becomes the repository's description, the Catalogue's and the README's.
  - name: team
    kind: resource
    kinds: [Team]
    title: Owning team
    required: true
    hint: The team that will look after it.
  - name: language
    kind: select
    title: Language
    required: true
    group: What to build
    default: go
    options:
      - value: go
        title: Go
      - value: rust
        title: Rust
      - value: python
        title: Python
      - value: cpp
        title: C++
      - value: typescript
        title: TypeScript
  - name: app
    kind: select
    title: What kind of application
    required: true
    group: What to build
    default: api-rest
    options:
      - value: cli
        title: CLI
      - value: api-rest
        title: API (REST)
      - value: api-grpc
        title: API (gRPC)
      - value: k8s-controller
        title: Kubernetes controller
      - value: k8s-crd
        title: Kubernetes CRDs
  - name: module
    title: Module path
    group: What to build
    hint: >-
      Where the code will be imported from, without the service's name: `github.com/acme` gives the
      Go module `github.com/acme/payments-api`. Leave it empty for `example.com`.
    placeholder: github.com/acme
  - name: resource
    title: Custom resource
    group: What to build
    hint: >-
      For the Kubernetes scaffolds: the Kind the CRD defines, such as `Ledger`. Leave it empty to
      name it after the service.
  - name: group
    title: API group
    group: What to build
    hint: >-
      For the Kubernetes scaffolds: the API group its CRD is in. Leave it empty for `example.com`.
    placeholder: acme.example
  - name: destination
    kind: select
    title: Where it goes
    group: Where it goes
    required: true
    default: github
    hint: >-
      A new repository on GitHub, created with this platform's token; or a download, a .zip or a
      .tar.gz on the run's page, which needs no git host at all.
    options:
      - value: github
        title: A new repository on GitHub
      - value: download
        title: A download, as .zip or .tar.gz
  - name: machine
    title: Stand a machine up for it
    group: Where it runs
    hint: >-
      The Infra template to ask for a machine from, such as `dev-vm`. Leave it empty and nothing is
      stood up. The machine is provisioned in the background, and once it answers, DNS points the
      service's name at its address - as long as **A name for each service** is on there.
    placeholder: dev-vm
  - name: owner
    title: GitHub organisation
    group: Where it goes
    hint: For a repository on GitHub. Leave this empty to use the one the plugin is configured with.
  - name: visibility
    kind: select
    title: Who can see the repository
    group: Where it goes
    hint: For a repository on GitHub.
    default: private
    options:
      - value: private
        title: Private
      - value: public
        title: Public
scaffold:
  language: "{{ values.language }}"
  app: "{{ values.app }}"
steps:
  - id: publish
    title: Create the repository
    if: "{{ values.destination == 'github' }}"
    action: publish
    with:
      owner: "{{ values.owner }}"
      repository: "{{ values.name }}"
      description: "{{ values.description }}"
      visibility: "{{ values.visibility }}"
      message: "Scaffolded by DOC: {{ values.language }} {{ values.app }}"
  - id: repository
    title: Put the repository in the Catalogue
    if: "{{ values.destination == 'github' }}"
    action: register
    with:
      kind: Repository
      name: "{{ steps.publish.repository }}"
      title: "{{ steps.publish.repository }}"
      description: "{{ values.description }}"
      owner: "{{ values.team }}"
  - id: service
    title: Put the service in the Catalogue
    action: register
    with:
      kind: Service
      name: "{{ values.name }}"
      title: "{{ values.name }}"
      description: "{{ values.description }}"
      owner: "{{ values.team }}"
      connections:
        - "{% if steps.publish.repository %}Repository:{{ steps.publish.repository }}{% endif %}"
      metadata:
        language: "{{ values.language }}"
        application: "{{ values.app }}"
        lifecycle: "{{ lifecycle.name }}"
        telemetry: "{{ telemetry.endpoint }}"
        environment: "{{ telemetry.environment }}"
  - id: machine
    title: Ask Infra for a machine
    if: "{{ values.machine }}"
    action: infra
    with:
      template: "{{ values.machine }}"
      team: "{{ values.team }}"
      service: "{{ values.name }}"
      name: "{{ values.name }}"
  - id: greeting
    title: Give it its first feature flag
    action: request
    with:
      plugin: flags
      method: POST
      route: entries
      body:
        role: flag
        key: greeting
        service: "{{ values.name }}"
        kind: string
        value: Hello
        description: "What {{ values.name }} says. The scaffold reads it, and falls back to `Hello`."
  - id: reconciling
    title: Give the controller its stop switch
    if: "{{ values.app == 'k8s-controller' }}"
    action: request
    with:
      plugin: flags
      method: POST
      route: entries
      body:
        role: flag
        key: reconcile
        service: "{{ values.name }}"
        kind: boolean
        value: true
        description: "Turn this off to have {{ values.name }} watch without changing anything."
  - id: tell
    title: Tell whoever asked
    action: notify
    with:
      title: "{{ values.name }} is ready"
      body: >-
        A {{ values.language }} {{ values.app }}{% if values.destination == 'github' %} at
        {{ steps.publish.url }}{% else %}, to download from its run{% endif %} - {{ lifecycle.title }},
        exporting to {{ telemetry.endpoint }} and reading its flags from DOC. Give it a service
        account with plugin:flags:service:ro to read them.
      url: "{% if values.destination == 'github' %}{{ steps.publish.url }}{% else %}/p/templates/runs/{{ run.id }}{% endif %}"
links:
  - title: The repository
    url: "{{ steps.publish.url }}"
  - title: In the Catalogue
    url: "{{ steps.service.url }}"
  - title: Its machine
    url: "{{ steps.machine.url }}"
  - title: Its feature flags
    url: "/p/flags/?service={{ values.name }}"
"#;

/// A command line tool rather than something that is deployed: the same five languages and the
/// same wiring, without the manifests for running it in a cluster.
const TOOL: &str = r#"
name: new-tool
title: New tool
description: >-
  Creates a command line tool from one of the platform's scaffolds - Go, Rust, Python, C++ or
  TypeScript - with its commands, its telemetry and its feature flags wired to this platform
  already. Nothing is deployed, so it is given no manifests: it is built, installed and run by
  whoever has it. It goes in the Catalogue with how far along it is. It can be a download instead of
  a repository, which needs no git host.
kind: Tool
tags: [tool, cli, scaffold, github]
parameters:
  - name: name
    title: What the tool is called
    hint: Lowercase letters, digits and dashes. It names the repository, the command and the package.
    required: true
    pattern: "^[a-z][a-z0-9-]{1,38}$"
    expects: A tool's name is 2 to 39 lowercase letters, digits and dashes, starting with a letter.
    placeholder: ledgerctl
  - name: description
    kind: textarea
    title: What it does
    required: true
    hint: A sentence or two. It becomes the repository's description, the Catalogue's and the README's.
  - name: team
    kind: resource
    kinds: [Team]
    title: Owning team
    required: true
    hint: The team that will look after it.
  - name: language
    kind: select
    title: Language
    required: true
    group: What to build
    default: go
    options:
      - value: go
        title: Go
      - value: rust
        title: Rust
      - value: python
        title: Python
      - value: cpp
        title: C++
      - value: typescript
        title: TypeScript
  - name: module
    title: Module path
    group: What to build
    hint: >-
      Where the code will be imported from, without the tool's name: `github.com/acme` gives the
      Go module `github.com/acme/ledgerctl`. Leave it empty for `example.com`.
    placeholder: github.com/acme
  - name: destination
    kind: select
    title: Where it goes
    group: Where it goes
    required: true
    default: github
    hint: >-
      A new repository on GitHub, created with this platform's token; or a download, a .zip or a
      .tar.gz on the run's page, which needs no git host at all.
    options:
      - value: github
        title: A new repository on GitHub
      - value: download
        title: A download, as .zip or .tar.gz
  - name: machine
    title: Stand a machine up for it
    group: Where it runs
    hint: >-
      The Infra template to ask for a machine from, such as `dev-vm`. Leave it empty and nothing is
      stood up. The machine is provisioned in the background, and once it answers, DNS points the
      service's name at its address - as long as **A name for each service** is on there.
    placeholder: dev-vm
  - name: owner
    title: GitHub organisation
    group: Where it goes
    hint: For a repository on GitHub. Leave this empty to use the one the plugin is configured with.
  - name: visibility
    kind: select
    title: Who can see the repository
    group: Where it goes
    hint: For a repository on GitHub.
    default: private
    options:
      - value: private
        title: Private
      - value: public
        title: Public
scaffold:
  language: "{{ values.language }}"
  app: cli
steps:
  - id: publish
    title: Create the repository
    if: "{{ values.destination == 'github' }}"
    action: publish
    with:
      owner: "{{ values.owner }}"
      repository: "{{ values.name }}"
      description: "{{ values.description }}"
      visibility: "{{ values.visibility }}"
      message: "Scaffolded by DOC: a {{ values.language }} command line tool"
  - id: repository
    title: Put the repository in the Catalogue
    if: "{{ values.destination == 'github' }}"
    action: register
    with:
      kind: Repository
      name: "{{ steps.publish.repository }}"
      title: "{{ steps.publish.repository }}"
      description: "{{ values.description }}"
      owner: "{{ values.team }}"
  - id: tool
    title: Put the tool in the Catalogue
    action: register
    with:
      kind: Service
      name: "{{ values.name }}"
      title: "{{ values.name }}"
      description: "{{ values.description }}"
      owner: "{{ values.team }}"
      connections:
        - "{% if steps.publish.repository %}Repository:{{ steps.publish.repository }}{% endif %}"
      metadata:
        language: "{{ values.language }}"
        application: cli
        lifecycle: "{{ lifecycle.name }}"
        telemetry: "{{ telemetry.endpoint }}"
        environment: "{{ telemetry.environment }}"
  - id: greeting
    title: Give it the flag its first command reads
    action: request
    with:
      plugin: flags
      method: POST
      route: entries
      body:
        role: flag
        key: greeting
        service: "{{ values.name }}"
        kind: string
        value: Hello
        description: "What `{{ values.name }} hello` says. The scaffold reads it, and falls back to `Hello`."
  - id: shout
    title: Give it its switch
    action: request
    with:
      plugin: flags
      method: POST
      route: entries
      body:
        role: flag
        key: shout
        service: "{{ values.name }}"
        kind: boolean
        value: false
        description: "Turn this on to have {{ values.name }} say it in capitals."
  - id: tell
    title: Tell whoever asked
    action: notify
    with:
      title: "{{ values.name }} is ready"
      body: >-
        A {{ values.language }} command line tool{% if values.destination == 'github' %} at
        {{ steps.publish.url }}{% else %}, to download from its run{% endif %} —
        {{ lifecycle.title }}. Its README says how to install it. It reads its flags from DOC with a
        service account holding plugin:flags:service:ro.
      url: "{% if values.destination == 'github' %}{{ steps.publish.url }}{% else %}/p/templates/runs/{{ run.id }}{% endif %}"
links:
  - title: The repository
    url: "{{ steps.publish.url }}"
  - title: In the Catalogue
    url: "{{ steps.tool.url }}"
  - title: Its feature flags
    url: "/p/flags/?service={{ values.name }}"
"#;

/// A plugin for this platform, in Go, by one of two paths: a tutorial that grows the smallest plugin
/// into one that talks to a person, another plugin and a weather service, or the boilerplate with
/// its metadata filled in and what to check before it is deployed.
const DOC_PLUGIN: &str = r#"
name: new-plugin
title: New DOC plugin
description: >-
  Creates a repository for a DOC plugin written in Go, with the plugin protocol already in it. Take
  the guided path to be walked through writing one from scratch - a page, settings, a weather
  service, a notification to a colleague and an event in DOC's calendar - or the unguided one for
  the boilerplate with its metadata filled in and the checks to make before it is deployed. It can
  be a download instead of a repository.
kind: Plugin
tags: [plugin, go, tutorial, scaffold, github]
parameters:
  - name: guidance
    kind: select
    title: How would you like to start?
    group: How to start
    required: true
    default: doc-plugin-tutorial
    hint: >-
      Guided gives you the smallest plugin there is and TUTORIAL.md, which takes about an hour to
      grow it into one that cheers a colleague up on cloudy days. Unguided gives you a working
      plugin - a page, an API, a setting, a collection and an operation - to make your own.
    options:
      - value: doc-plugin-tutorial
        title: Guided, with a tutorial
      - value: doc-plugin
        title: Unguided, the boilerplate
  - name: name
    title: Plugin ID
    group: About the plugin
    required: true
    pattern: "^[a-z][a-z0-9-]{1,31}$"
    expects: >-
      A plugin ID is 2 to 32 lowercase letters, digits and dashes, starting with a letter.
    hint: >-
      How DOC and every other plugin address it, for good. It names the repository and the binary
      too, and whoever runs DOC adds it to the platform's plugin IDs before it can register.
    placeholder: cloudy-cheer
  - name: title
    title: What the navigation calls it
    group: About the plugin
    required: true
    placeholder: Cloudy day cheer
  - name: description
    kind: textarea
    title: What it does
    group: About the plugin
    required: true
    hint: >-
      A sentence or two, shown on its card on DOC's landing page, and in the repository's
      description and README.
  - name: menu
    kind: select
    title: Which menu it goes in
    group: About the plugin
    default: Workspace
    options: [Workspace, Technology, Platform, Watercooler, Admin]
  - name: classification
    kind: select
    title: What its run is for
    group: About the plugin
    default: synchronous
    hint: >-
      For the unguided path; the tutorial's plugin is synchronous. It can be changed later, in the
      manifest.
    options:
      - value: synchronous
        title: Synchronous, run on demand while the caller waits
      - value: async
        title: Async, run on demand in the background
      - value: one-shot
        title: One-shot, run once each time it is loaded
      - value: long-running
        title: Long-running, running for as long as it is loaded
  - name: team
    kind: resource
    kinds: [Team]
    title: Owning team
    group: About the plugin
    required: true
  - name: module
    title: Module path
    group: Where it goes
    hint: >-
      Where the code will be imported from, without the plugin's ID: `github.com/acme` gives the Go
      module `github.com/acme/cloudy-cheer`. Leave it empty for `example.com`.
    placeholder: github.com/acme
  - name: destination
    kind: select
    title: Where it goes
    group: Where it goes
    required: true
    default: github
    hint: >-
      A new repository on GitHub, created with this platform's token; or a download, a .zip or a
      .tar.gz on the run's page, which needs no git host at all.
    options:
      - value: github
        title: A new repository on GitHub
      - value: download
        title: A download, as .zip or .tar.gz
  - name: machine
    title: Stand a machine up for it
    group: Where it runs
    hint: >-
      The Infra template to ask for a machine from, such as `dev-vm`. Leave it empty and nothing is
      stood up. The machine is provisioned in the background, and once it answers, DNS points the
      service's name at its address - as long as **A name for each service** is on there.
    placeholder: dev-vm
  - name: owner
    title: GitHub organisation
    group: Where it goes
    hint: For a repository on GitHub. Leave this empty to use the one the plugin is configured with.
  - name: visibility
    kind: select
    title: Who can see the repository
    group: Where it goes
    hint: For a repository on GitHub.
    default: private
    options:
      - value: private
        title: Private
      - value: public
        title: Public
scaffold:
  language: go
  app: "{{ values.guidance }}"
steps:
  - id: publish
    title: Create the repository
    if: "{{ values.destination == 'github' }}"
    action: publish
    with:
      owner: "{{ values.owner }}"
      repository: "{{ values.name }}"
      description: "{{ values.description }}"
      visibility: "{{ values.visibility }}"
      message: "Created by DOC: a DOC plugin in Go{% if values.guidance == 'doc-plugin-tutorial' %}, with its tutorial{% endif %}"
  - id: repository
    title: Put the repository in the Catalogue
    if: "{{ values.destination == 'github' }}"
    action: register
    with:
      kind: Repository
      name: "{{ steps.publish.repository }}"
      title: "{{ steps.publish.repository }}"
      description: "{{ values.description }}"
      owner: "{{ values.team }}"
  - id: plugin
    title: Put the plugin in the Catalogue
    action: register
    with:
      kind: Service
      name: "{{ values.name }}"
      title: "{{ values.title }}"
      description: "{{ values.description }}"
      owner: "{{ values.team }}"
      connections:
        - "{% if steps.publish.repository %}Repository:{{ steps.publish.repository }}{% endif %}"
      metadata:
        language: go
        application: doc-plugin
        plugin: "{{ values.name }}"
        lifecycle: "{{ lifecycle.name }}"
  - id: tell
    title: Tell whoever asked
    action: notify
    with:
      title: "{{ values.name }} is ready"
      body: >-
        {% if values.destination == 'download' %}It is ready to download from its run. {% endif %}{%
        if values.guidance == 'doc-plugin-tutorial' %}Its tutorial is TUTORIAL.md.{% else %}Its
        README says how to run it and what to check before it is deployed.{% endif %} Before it
        can register, whoever runs DOC adds {{ values.name }} to the platform's plugin IDs.
      url: "{% if values.destination == 'github' %}{{ steps.publish.url }}{% else %}/p/templates/runs/{{ run.id }}{% endif %}"
links:
  - title: The tutorial
    url: "{% if values.guidance == 'doc-plugin-tutorial' %}{% if steps.publish.url %}{{ steps.publish.url }}/blob/{{ steps.publish.branch }}/TUTORIAL.md{% endif %}{% endif %}"
  - title: What to check before it is deployed
    url: "{% if values.guidance == 'doc-plugin' %}{% if steps.publish.url %}{{ steps.publish.url }}/blob/{{ steps.publish.branch }}/README.md#verification{% endif %}{% endif %}"
  - title: The repository
    url: "{{ steps.publish.url }}"
  - title: In the Catalogue
    url: "{{ steps.plugin.url }}"
"#;

pub const BUILT_IN: [&str; 3] = [SERVICE, TOOL, DOC_PLUGIN];

/// Who a built-in template belongs to until somebody edits it, which is how the plugin knows one
/// it may put back the way it wrote it.
pub const PLUGIN: &str = "the plugin";

/// Puts the built-in templates in place, and keeps the ones nobody has touched up to date with the
/// version of the plugin that is running. One an author has edited is theirs from then on and is
/// left alone, and one they have removed stays removed until the plugin loads again.
pub async fn seed(backend: &Backend) -> Result<usize, Refusal> {
    let store = Store(backend);
    let mut written = 0;
    let mut shipped: Vec<String> = Vec::new();
    for source in BUILT_IN {
        let definition = model::definition(source).map_err(|err| {
            Refusal::unavailable(format!("a built-in template is not one: {err}"))
        })?;
        shipped.push(definition.name.clone());
        match store.template(&definition.name).await? {
            Some(stored) if !stored.builtin || stored.author != PLUGIN => continue,
            Some(stored)
                if stored.definition == serde_json::to_value(&definition).unwrap_or_default() =>
            {
                continue;
            }
            _ => {}
        }
        store.save(&definition, PLUGIN, true).await?;
        written += 1;
    }
    // One this version no longer ships goes, unless somebody has made it theirs by editing it.
    for stored in store.templates(None).await? {
        if stored.builtin && stored.author == PLUGIN && !shipped.contains(&stored.name) {
            store.remove(&stored.name).await?;
            written += 1;
        }
    }
    Ok(written)
}
