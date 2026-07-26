//! The scaffolds the plugin carries: a language, an application type, and the files that make a
//! service of that shape. Every one of them is wired to this platform's telemetry stack and to its
//! feature flags before anybody edits a line, which is the point of scaffolding it here rather than
//! copying a repository that was wired up once, a year ago.

include!(concat!(env!("OUT_DIR"), "/scaffolds.rs"));

/// Files common to every application of a language.
const BASE: &str = "base";
/// Files common to every DOC plugin of a language: the plugin protocol, its build and its image.
const PLUGIN_BASE: &str = "plugin-base";
/// In a scaffold's path: the service's name, and the name as a package or module is written.
const NAME: &str = "_NAME_";
const PACKAGE: &str = "_PACKAGE_";

pub struct Language {
    pub name: &'static str,
    pub title: &'static str,
    /// What the card and the README say it is built with.
    pub about: &'static str,
}

pub struct App {
    pub name: &'static str,
    pub title: &'static str,
    pub about: &'static str,
    /// What an application of this kind has no use for, as path prefixes of the language's base:
    /// a command line tool is not deployed, so it is not given manifests to deploy it with.
    pub without: &'static [&'static str],
    /// The files it starts from: the language's `base`, or a part of its own that it shares with
    /// its kind, as a DOC plugin shares the protocol with the tutorial's.
    pub base: &'static str,
    /// The languages it is written in; empty for every one.
    pub languages: &'static [&'static str],
}

pub const LANGUAGES: [Language; 5] = [
    Language { name: "go", title: "Go", about: "Go 1.24, the standard library and OpenTelemetry" },
    Language {
        name: "rust",
        title: "Rust",
        about: "Rust 2024, tokio and OpenTelemetry through tracing",
    },
    Language {
        name: "python",
        title: "Python",
        about: "Python 3.12, uv or pip, and OpenTelemetry",
    },
    Language { name: "cpp", title: "C++", about: "C++20, CMake and the OpenTelemetry C++ SDK" },
    Language {
        name: "typescript",
        title: "TypeScript",
        about: "TypeScript on Node 22 and OpenTelemetry",
    },
];

pub const APPS: [App; 7] = [
    App {
        name: "cli",
        title: "CLI",
        about: "A command line program, with its commands, its flags and its own telemetry",
        without: &["deploy/"],
        base: BASE,
        languages: &[],
    },
    App {
        name: "api-rest",
        title: "API (REST)",
        about: "An HTTP service with health, readiness and a versioned JSON API",
        without: &[],
        base: BASE,
        languages: &[],
    },
    App {
        name: "api-grpc",
        title: "API (gRPC)",
        about: "A gRPC service with its protobuf contract, reflection and health checks",
        without: &[],
        base: BASE,
        languages: &[],
    },
    App {
        name: "k8s-controller",
        title: "Kubernetes controller",
        about: "A controller that reconciles a resource, with its RBAC and its deployment",
        without: &[],
        base: BASE,
        languages: &[],
    },
    App {
        name: "k8s-crd",
        title: "Kubernetes CRDs",
        about: "A custom resource: its schema, its generated types and a sample",
        without: &[],
        base: BASE,
        languages: &[],
    },
    App {
        name: "doc-plugin",
        title: "DOC plugin",
        about: "A plugin for this platform: its manifest, a page, an API route and a background \
                run, speaking the plugin protocol",
        without: &[],
        base: PLUGIN_BASE,
        languages: &["go"],
    },
    App {
        name: "doc-plugin-tutorial",
        title: "DOC plugin, with a tutorial",
        about: "The smallest DOC plugin and a tutorial that builds it into one that talks to a \
                person, another plugin and a weather service",
        without: &[],
        base: PLUGIN_BASE,
        languages: &["go"],
    },
];

pub fn language(name: &str) -> Option<&'static Language> {
    LANGUAGES.iter().find(|language| language.name == name)
}

pub fn app(name: &str) -> Option<&'static App> {
    APPS.iter().find(|app| app.name == name)
}

/// The files of one scaffold: its base's, then the application's, which replace a base file of the
/// same path. Paths still hold `_NAME_` and `_PACKAGE_`; `named` fills those in.
pub fn files(language: &str, app: &str) -> Vec<(String, &'static str)> {
    let (dropped, base) =
        self::app(app).map_or((&[] as &[&str], BASE), |app| (app.without, app.base));
    let mut chosen: Vec<(String, &'static str)> = Vec::new();
    for part in [base, app] {
        for (its_language, its_part, path, content) in FILES {
            if *its_language != language || *its_part != part {
                continue;
            }
            if part == base && dropped.iter().any(|prefix| path.starts_with(prefix)) {
                continue;
            }
            match chosen.iter_mut().find(|(held, _)| held == path) {
                Some(held) => held.1 = content,
                None => chosen.push(((*path).to_string(), content)),
            }
        }
    }
    chosen
}

/// A scaffold's path with the service's name in it: `src/_PACKAGE_/main.py` becomes
/// `src/payments_api/main.py`.
pub fn named(path: &str, name: &str, package: &str) -> String {
    path.replace(PACKAGE, package).replace(NAME, name)
}

/// Whether a pair is scaffolded, which is checked before a template is stored: the application is
/// written in that language, and has files of its own in it.
pub fn holds(language: &str, app: &str) -> bool {
    let Some(found) = self::app(app) else { return false };
    self::language(language).is_some()
        && (found.languages.is_empty() || found.languages.contains(&language))
        && FILES
            .iter()
            .any(|(its_language, its_part, _, _)| *its_language == language && *its_part == app)
}
