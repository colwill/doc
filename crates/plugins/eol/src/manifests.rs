//! What a repository's own files say it is built on: the runtimes, databases and operating systems
//! endoflife.date tracks, read from the files a project already keeps rather than from anything
//! added for DOC. A version is taken only where the file states one plainly; a range or a
//! constraint is read down to the release it pins where it names one, and left out where it does
//! not, because a wrong version is worse here than none.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One product a repository's files say it uses, and the file that said so.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Found {
    /// endoflife.date's name for it.
    pub product: String,
    #[serde(default)]
    pub version: Option<String>,
    /// The file it was read from, relative to the repository's root.
    pub file: String,
}

/// The longest file read: a manifest past this is not one.
const MAX_FILE: usize = 512 * 1024;
/// The most workflow files looked at, since a repository may hold a great many.
const MAX_WORKFLOWS: usize = 40;
/// The most project files of one kind looked at (`*.csproj`, `Dockerfile.*`).
const MAX_OF_A_KIND: usize = 20;

/// A container image's repository, as endoflife.date names what is in it. Only images whose tag
/// *is* the product's version are here: `node:20` is Node.js 20, but `myorg/api:1.4` is not a
/// product anybody tracks.
const IMAGES: [(&str, &str); 36] = [
    ("node", "nodejs"),
    ("python", "python"),
    ("golang", "go"),
    ("ruby", "ruby"),
    ("php", "php"),
    // The image's own name is the distribution, which is what endoflife.date tracks: there is no
    // product for Java on its own. `openjdk` is the retired official image, whose tags line up
    // with Temurin's, which is what replaced it.
    ("openjdk", "eclipse-temurin"),
    ("eclipse-temurin", "eclipse-temurin"),
    ("amazoncorretto", "amazon-corretto"),
    ("rust", "rust"),
    ("elixir", "elixir"),
    ("erlang", "erlang"),
    ("perl", "perl"),
    ("postgres", "postgresql"),
    ("mysql", "mysql"),
    ("mariadb", "mariadb"),
    ("redis", "redis"),
    ("mongo", "mongodb"),
    ("cassandra", "apache-cassandra"),
    ("elasticsearch", "elasticsearch"),
    ("opensearchproject/opensearch", "opensearch"),
    ("rabbitmq", "rabbitmq"),
    ("consul", "consul"),
    ("hashicorp/consul", "consul"),
    ("vault", "hashicorp-vault"),
    ("hashicorp/vault", "hashicorp-vault"),
    ("nginx", "nginx"),
    ("httpd", "apache-http-server"),
    ("haproxy", "haproxy"),
    ("traefik", "traefik"),
    ("grafana/grafana", "grafana"),
    ("alpine", "alpine-linux"),
    ("ubuntu", "ubuntu"),
    ("debian", "debian"),
    ("rockylinux", "rocky-linux"),
    ("almalinux", "almalinux"),
    ("amazonlinux", "amazon-linux"),
];
/// Images whose own name says the product, under a registry path.
const IMAGE_PATHS: [(&str, &str); 5] = [
    ("mcr.microsoft.com/dotnet/aspnet", "dotnet"),
    ("mcr.microsoft.com/dotnet/sdk", "dotnet"),
    ("mcr.microsoft.com/dotnet/runtime", "dotnet"),
    ("confluentinc/cp-kafka", "apache-kafka"),
    ("bitnami/kafka", "apache-kafka"),
];
/// What `.tool-versions` and `mise.toml` call a thing, where it is not endoflife.date's name.
/// What `.tool-versions`, `mise.toml` and `runtime.txt` call a thing, where it is not
/// endoflife.date's name. `java` is not among them: endoflife.date tracks the JDK distributions —
/// Temurin, Corretto, Oracle's — and a version alone does not say which is being run.
const TOOLS: [(&str, &str); 9] = [
    ("nodejs", "nodejs"),
    ("node", "nodejs"),
    ("python", "python"),
    ("ruby", "ruby"),
    ("golang", "go"),
    ("go", "go"),
    ("rust", "rust"),
    ("terraform", "terraform"),
    ("elixir", "elixir"),
];
/// Directories holding somebody else's code, or something built: what is in them is not what this
/// repository runs, and a vendored `node_modules` alone would be thousands of manifests.
const VENDORED: [&str; 12] = [
    "node_modules",
    "vendor",
    "third_party",
    "thirdparty",
    "target",
    "dist",
    "build",
    "site-packages",
    "testdata",
    "fixtures",
    "examples",
    "test",
];

/// Whether a path is one of the files read, so an archive keeps only those and nothing else is
/// held in memory. Matched on the path as it is in the repository, without its top directory.
pub fn wanted(path: &str) -> bool {
    let mut parts = path.split('/').peekable();
    while let Some(part) = parts.next() {
        // The last part is the file's own name, which may legitimately be `test` or `build`.
        if parts.peek().is_some()
            && (part.starts_with('.') && part != ".github" || VENDORED.contains(&part))
        {
            return false;
        }
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    let lower = name.to_ascii_lowercase();
    let workflow = (path.starts_with(".github/workflows/") || path.starts_with(".gitlab-ci"))
        && (lower.ends_with(".yml") || lower.ends_with(".yaml"));
    workflow
        || lower.starts_with("dockerfile")
        || lower.starts_with("docker-compose")
        || lower.ends_with(".csproj")
        || lower.ends_with(".fsproj")
        || matches!(
            lower.as_str(),
            ".nvmrc"
                | ".node-version"
                | ".python-version"
                | ".ruby-version"
                | ".tool-versions"
                | "mise.toml"
                | ".mise.toml"
                | "package.json"
                | "go.mod"
                | "cargo.toml"
                | "rust-toolchain"
                | "rust-toolchain.toml"
                | "pyproject.toml"
                | "runtime.txt"
                | "gemfile"
                | "composer.json"
                | "pom.xml"
                | "global.json"
        )
}

fn text(bytes: &[u8]) -> Option<String> {
    (bytes.len() <= MAX_FILE).then(|| String::from_utf8_lossy(bytes).into_owned())
}

/// A version as it is written down to what endoflife.date would call a release: `v20.11.1` is
/// `20.11.1`, `lts/*` and `stable` name no release at all.
fn version(written: &str) -> Option<String> {
    let cleaned = written
        .trim()
        .trim_start_matches(['v', 'V'])
        .trim_matches(|c: char| c == '"' || c == '\'' || c.is_whitespace());
    let kept: String =
        cleaned.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect::<String>();
    let kept = kept.trim_end_matches('.').to_string();
    (!kept.is_empty() && kept.starts_with(|c: char| c.is_ascii_digit())).then_some(kept)
}

/// The lowest version a constraint allows, which is the one being run: `>=3.11`, `^20.1.0`,
/// `~> 1.5`, `3.11.*`. A constraint with no number in it names nothing.
fn floor(constraint: &str) -> Option<String> {
    let first = constraint
        .split([',', '|'])
        .map(str::trim)
        .find(|part| !part.starts_with('<') && !part.starts_with("!="))
        .unwrap_or(constraint);
    version(first.trim_start_matches(['>', '=', '^', '~', ' ', '@']))
}

/// A container image reference as the product in it: `node:20-alpine` is Node.js 20, and an image
/// nobody tracks is nothing. A tag with no version in it, such as `latest`, names no release.
fn image(reference: &str) -> Option<(String, Option<String>)> {
    let reference = reference.trim().trim_matches(|c: char| c == '"' || c == '\'');
    // A digest says nothing about which release it is.
    let reference = reference.split('@').next()?;
    let (name, tag) = match reference.rsplit_once(':') {
        // A registry's port, not a tag.
        Some((name, tag)) if !tag.contains('/') => (name, Some(tag)),
        _ => (reference, None),
    };
    let name = name.trim_start_matches("docker.io/").trim_start_matches("library/");
    let product = IMAGE_PATHS
        .iter()
        .find(|(path, _)| name.eq_ignore_ascii_case(path))
        .or_else(|| IMAGES.iter().find(|(image, _)| name.eq_ignore_ascii_case(image)))
        .map(|(_, product)| (*product).to_string())?;
    // `node:20-alpine` runs Alpine too, but which release it is the image's business, not the
    // Dockerfile's, so only the named product is taken.
    Some((product, tag.and_then(version)))
}

/// The value of a line written `key = value`, `key: value` or `key value`, unquoted.
fn value_after(line: &str, key: &str) -> Option<String> {
    let rest = line.trim().strip_prefix(key)?;
    let rest = rest.trim_start().trim_start_matches(['=', ':']).trim();
    let rest = rest.split(['#', ';']).next().unwrap_or(rest).trim();
    let rest = rest.trim_matches(|c: char| c == '"' || c == '\'');
    (!rest.is_empty()).then(|| rest.to_string())
}

/// The value of a line whose first word is exactly `key`: `go 1.21`, `ruby "3.2"`. Written this
/// way so `go.mod`'s `go` directive is not read out of `google.golang.org/grpc`.
fn directive(line: &str, key: &str) -> Option<String> {
    let line = line.trim();
    let (first, rest) = line.split_at_checked(key.len())?;
    if !first.eq_ignore_ascii_case(key) || !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let rest = rest.trim();
    let rest = rest.split(['#', ';']).next().unwrap_or(rest).trim();
    let rest = rest.trim_matches(|c: char| c == '"' || c == '\'');
    (!rest.is_empty()).then(|| rest.to_string())
}

/// The text inside `<tag>…</tag>`, the first one.
fn element(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    let inner = xml[start..end].trim();
    (!inner.is_empty()).then(|| inner.to_string())
}

/// A JSON field's string value, without parsing the whole document: `"node": ">=20"`.
fn json_field(document: &serde_json::Value, path: &[&str]) -> Option<String> {
    let mut at = document;
    for step in path {
        at = at.get(step)?;
    }
    match at {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// Everything one file says.
fn from_file(path: &str, bytes: &[u8]) -> Vec<(String, Option<String>)> {
    let Some(body) = text(bytes) else { return Vec::new() };
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    let mut found: Vec<(String, Option<String>)> = Vec::new();
    let mut add = |product: &str, version: Option<String>| {
        found.push((product.to_string(), version));
    };
    match name.as_str() {
        ".nvmrc" | ".node-version" => add("nodejs", version(&body)),
        ".python-version" => {
            for line in body.lines() {
                if let Some(read) = version(line) {
                    add("python", Some(read));
                }
            }
        }
        ".ruby-version" => add("ruby", version(&body)),
        "runtime.txt" => {
            // Heroku's: `python-3.11.4`, `nodejs-20.11.1`.
            if let Some((tool, rest)) = body.trim().split_once('-')
                && let Some(product) = TOOLS.iter().find(|(named, _)| *named == tool)
            {
                add(product.1, version(rest));
            }
        }
        ".tool-versions" => {
            for line in body.lines().filter(|line| !line.trim_start().starts_with('#')) {
                let mut words = line.split_whitespace();
                let (Some(tool), Some(written)) = (words.next(), words.next()) else { continue };
                if let Some((_, product)) = TOOLS.iter().find(|(named, _)| *named == tool) {
                    // `java adoptopenjdk-17.0.1` names the build before the version.
                    let written = written.rsplit('-').next().unwrap_or(written);
                    add(product, version(written));
                }
            }
        }
        "mise.toml" | ".mise.toml" => {
            let mut tools = false;
            for line in body.lines() {
                let line = line.trim();
                if line.starts_with('[') {
                    tools = line == "[tools]";
                    continue;
                }
                let Some((tool, written)) = line.split_once('=') else { continue };
                if tools && let Some((_, product)) = TOOLS.iter().find(|(n, _)| *n == tool.trim()) {
                    add(product, floor(written));
                }
            }
        }
        "package.json" => {
            if let Ok(document) = serde_json::from_str::<serde_json::Value>(&body) {
                if let Some(engine) = json_field(&document, &["engines", "node"]) {
                    add("nodejs", floor(&engine));
                }
                for (dependency, product) in
                    [("next", "nextjs"), ("nuxt", "nuxt"), ("@angular/core", "angular")]
                {
                    if let Some(version) = json_field(&document, &["dependencies", dependency]) {
                        add(product, floor(&version));
                    }
                }
            }
        }
        "go.mod" => {
            for line in body.lines() {
                if let Some(read) = directive(line, "go") {
                    add("go", version(&read));
                    break;
                }
            }
        }
        "cargo.toml" => {
            for line in body.lines() {
                if let Some(read) = value_after(line, "rust-version") {
                    add("rust", version(&read));
                    break;
                }
            }
        }
        "rust-toolchain" | "rust-toolchain.toml" => {
            let written = body
                .lines()
                .find_map(|line| value_after(line, "channel"))
                .unwrap_or_else(|| body.trim().to_string());
            add("rust", version(&written));
        }
        "pyproject.toml" => {
            for line in body.lines() {
                if let Some(read) = value_after(line, "requires-python") {
                    add("python", floor(&read));
                    break;
                }
            }
        }
        "gemfile" => {
            for line in body.lines() {
                if let Some(read) = directive(line, "ruby") {
                    add("ruby", floor(&read));
                    break;
                }
            }
        }
        "composer.json" => {
            if let Ok(document) = serde_json::from_str::<serde_json::Value>(&body) {
                if let Some(read) = json_field(&document, &["require", "php"]) {
                    add("php", floor(&read));
                }
                if let Some(read) = json_field(&document, &["require", "laravel/framework"]) {
                    add("laravel", floor(&read));
                }
                if let Some(read) = json_field(&document, &["require", "symfony/framework-bundle"])
                {
                    add("symfony", floor(&read));
                }
            }
        }
        // Only Spring Boot: the Java version a POM compiles against does not say which JDK
        // distribution is run, and endoflife.date has a product for each distribution rather than
        // one for Java.
        "pom.xml" => {
            if body.contains("spring-boot-starter-parent")
                && let Some(read) = body
                    .split("spring-boot-starter-parent")
                    .nth(1)
                    .and_then(|rest| element(rest, "version"))
            {
                add("spring-boot", version(&read));
            }
        }
        "global.json" => {
            if let Ok(document) = serde_json::from_str::<serde_json::Value>(&body)
                && let Some(read) = json_field(&document, &["sdk", "version"])
            {
                add("dotnet", version(&read));
            }
        }
        _ if name.ends_with(".csproj") || name.ends_with(".fsproj") => {
            if let Some(read) =
                element(&body, "TargetFramework").or_else(|| element(&body, "TargetFrameworks"))
            {
                // `net8.0`, or `net8.0;net6.0`.
                for framework in read.split(';') {
                    if let Some(read) = framework.trim().strip_prefix("net").and_then(version) {
                        add("dotnet", Some(read));
                    }
                }
            }
        }
        _ if name.starts_with("dockerfile") => {
            for line in body.lines() {
                // `FROM node:20 AS build`, and `--platform=` before the image.
                let Some(rest) = directive(line, "FROM") else { continue };
                let reference = rest
                    .split_whitespace()
                    .find(|word| !word.starts_with("--"))
                    .unwrap_or_default();
                if let Some((product, version)) = image(reference) {
                    add(&product, version);
                }
            }
        }
        _ if name.starts_with("docker-compose") => {
            for line in body.lines() {
                if let Some(read) = value_after(line.trim(), "image")
                    && let Some((product, version)) = image(&read)
                {
                    add(&product, version);
                }
            }
        }
        // A workflow file: what it runs on, and the images its services use.
        _ => {
            for line in body.lines() {
                let line = line.trim().trim_start_matches("- ").trim();
                if let Some(read) = value_after(line, "runs-on") {
                    // `ubuntu-22.04`, `ubuntu-latest`, `windows-2022`.
                    if let Some(release) = read.strip_prefix("ubuntu-").and_then(version) {
                        add("ubuntu", Some(release));
                    }
                } else if let Some(read) = value_after(line, "image")
                    && let Some((product, version)) = image(&read)
                {
                    add(&product, version);
                }
            }
        }
    }
    found
}

/// Everything a repository's files say it is built on, each product once per version, naming the
/// files it was read from. Files are read in path order, so the answer does not move about.
pub fn products(files: &BTreeMap<String, Vec<u8>>) -> Vec<Found> {
    let (mut workflows, mut of_a_kind) = (0usize, BTreeMap::new());
    let mut found: BTreeMap<(String, Option<String>), String> = BTreeMap::new();
    for (path, bytes) in files {
        if path.starts_with(".github/workflows/") {
            workflows += 1;
            if workflows > MAX_WORKFLOWS {
                continue;
            }
        }
        let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
        if name.starts_with("dockerfile") || name.ends_with(".csproj") || name.ends_with(".fsproj")
        {
            let kind = of_a_kind.entry(name.starts_with("dockerfile")).or_insert(0usize);
            *kind += 1;
            if *kind > MAX_OF_A_KIND {
                continue;
            }
        }
        for (product, version) in from_file(path, bytes) {
            found.entry((product, version)).or_insert_with(|| path.clone());
        }
    }
    found.into_iter().map(|((product, version), file)| Found { product, version, file }).collect()
}
