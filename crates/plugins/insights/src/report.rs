//! ccc's reports, cut down to what the pages show. A repository's report is one record, and a
//! record is at most 1 MiB, so every list is capped, and the caps are halved until it fits. A small
//! summary is kept beside it for lists and trends.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

/// The insights payload this plugin reads, and nothing else.
pub const SCHEMA: &str = "ccc-insights/v1";
const MAX_BYTES: usize = 900 * 1024;

/// What ccc answered for one repository: `insights` always, the other two when they ran.
pub struct Reports {
    pub insights: Value,
    pub sast: Result<Value, String>,
    pub audit: Result<Value, String>,
}

pub struct Condensed {
    pub summary: Value,
    pub report: Value,
}

pub fn condense(reports: &Reports) -> Result<Condensed, String> {
    if reports.insights["schema"] != SCHEMA {
        let said = reports.insights["schema"].as_str().unwrap_or("none");
        return Err(format!("ccc answered with schema {said}, and this plugin reads {SCHEMA}"));
    }
    let mut scale = 1;
    loop {
        let report = report(reports, scale);
        let size = serde_json::to_vec(&report).map_or(usize::MAX, |bytes| bytes.len());
        if size <= MAX_BYTES || scale >= 64 {
            return Ok(Condensed { summary: summary(&report), report });
        }
        scale *= 2;
    }
}

/// The keys of `value` named in `keys`, and only those.
fn pick(value: &Value, keys: &[&str]) -> Value {
    let mut picked = Map::new();
    for key in keys {
        if let Some(found) = value.get(*key) {
            picked.insert((*key).to_string(), found.clone());
        }
    }
    Value::Object(picked)
}

fn list(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).unwrap_or_default()
}

/// At most `cap` of `value`'s items, each cut to `keys`.
fn rows(value: &Value, cap: usize, keys: &[&str]) -> Value {
    Value::Array(list(value).iter().take(cap).map(|row| pick(row, keys)).collect())
}

/// At most `cap` of a list of plain values.
fn first(value: &Value, cap: usize) -> Value {
    Value::Array(list(value).iter().take(cap).cloned().collect())
}

fn number(value: &Value) -> u64 {
    value.as_u64().unwrap_or_default()
}

fn report(reports: &Reports, scale: usize) -> Value {
    let cap = |n: usize| (n / scale).max(5);
    let insights = &reports.insights;
    let function = [
        "name",
        "file",
        "line",
        "callers",
        "call_sites",
        "calls",
        "lines",
        "complexity",
        "loop_depth",
        "recursive",
        "language",
    ];
    let hot = &insights["hot"];
    let chains: Vec<Value> = list(&hot["deepest_chains"])
        .iter()
        .take(cap(15))
        .map(|chain| {
            json!({
                "depth": chain["depth"],
                "call_sites": chain["call_sites"],
                "chain": rows(&chain["chain"], 16, &["name", "file", "line"]),
            })
        })
        .collect();
    let cycles: Vec<Value> = list(&hot["cycles"])
        .iter()
        .take(cap(25))
        .map(|cycle| json!({ "size": cycle["size"], "members": rows(&cycle["members"], 12, &["name", "file", "line"]) }))
        .collect();

    let services = &insights["services"];
    let edges: Vec<Value> = list(&services["edges"])
        .iter()
        .take(cap(200))
        .map(|edge| {
            json!({
                "from": edge["from"],
                "to": edge["to"],
                "declared": edge["declared"],
                "detected": edge["detected"],
                "count": edge["count"],
                "symbols": first(&edge["symbols"], 12),
            })
        })
        .collect();
    let unassigned = list(&services["unassigned_files"]);

    let complexity: Vec<Value> = list(&insights["complexity"]["functions"])
        .iter()
        .filter(|row| row["test"] != true)
        .take(cap(100))
        .map(|row| {
            pick(
                row,
                &[
                    "function",
                    "file",
                    "line",
                    "language",
                    "service",
                    "complexity",
                    "params",
                    "loop_depth",
                    "lines",
                    "recursive",
                ],
            )
        })
        .collect();

    let lints = &insights["lints"];
    let mut by_rule: BTreeMap<String, u64> = BTreeMap::new();
    for finding in list(&lints["findings"]) {
        *by_rule.entry(finding["rule"].as_str().unwrap_or("other").to_string()).or_default() += 1;
    }

    let targets: Vec<Value> = list(&insights["test_targets"]["targets"])
        .iter()
        .take(cap(60))
        .map(|target| {
            let mut row = pick(
                target,
                &["function", "file", "line", "language", "service", "kind", "covered", "priority"],
            );
            row["covered_by"] = first(&target["covered_by"], 5);
            row
        })
        .collect();

    json!({
        "generated": insights["generated"],
        "took_ns": insights["took_ns"],
        "totals": insights["totals"],
        "languages": insights["languages"],
        "hot": {
            "most_called": rows(&hot["most_called"], cap(25), &function),
            "most_complex": rows(&hot["most_complex"], cap(25), &function),
            "widest": rows(&hot["widest"], cap(25), &function),
            "deepest_chains": chains,
            "cycles": cycles,
            "cycles_total": list(&hot["cycles"]).len(),
        },
        "services": {
            "source": services["source"],
            "services": list(&services["services"]).iter().take(cap(100)).map(|service| json!({
                "name": service["name"],
                "globs": first(&service["globs"], 10),
                "files": service["files"],
                "funcs": service["funcs"],
            })).collect::<Vec<_>>(),
            "edges": edges,
            "unassigned": unassigned.len(),
            "unassigned_files": Value::Array(unassigned.iter().take(cap(50)).cloned().collect()),
        },
        "complexity": { "total": insights["complexity"]["total"], "functions": complexity },
        "lints": {
            "truncated": lints["truncated"],
            "by_rule": by_rule,
            "rules": rows(&lints["rules"], 32, &["rule", "severity", "what", "limits"]),
            "findings": rows(&lints["findings"], cap(300), &["rule", "severity", "file", "line", "function", "message", "hint"]),
        },
        "tests": {
            "summary": insights["test_targets"]["summary"],
            "targets": targets,
        },
        "changes": changes(&insights["changes"], &cap),
        "security": security(&reports.sast, &cap),
        "dependencies": dependencies(&reports.audit, &cap),
    })
}

/// What changed since the scan before, when ccc could work it out.
fn changes(changes: &Value, cap: &dyn Fn(usize) -> usize) -> Value {
    if changes["available"] == false || !changes.is_object() {
        return json!({
            "available": false,
            "reason": changes["reason"].as_str().unwrap_or("ccc did not work out a change set"),
        });
    }
    let function = |value: &Value, n: usize| -> Value {
        Value::Array(
            list(value)
                .iter()
                .take(n)
                .map(|row| {
                    let mut kept = pick(row, &["file", "function", "lines", "tested", "services"]);
                    kept["tested_by"] = first(&row["tested_by"], 5);
                    kept["called_from"] = first(&row["called_from"], 5);
                    kept
                })
                .collect(),
        )
    };
    let edges: Vec<Value> = list(&changes["edges"])
        .iter()
        .take(cap(50))
        .map(|edge| {
            json!({
                "from": edge["from"],
                "to": edge["to"],
                "declared": edge["declared"],
                "detected": edge["detected"],
                "symbols": list(&edge["symbols"]).len(),
            })
        })
        .collect();
    json!({
        "available": true,
        "base_sha": changes["base_sha"],
        "head_sha": changes["head_sha"],
        "services": changes["services"],
        "services_to_test": changes["services_to_test"],
        "counts": changes["counts"],
        "changed_files": rows(&changes["changed_files"], cap(300), &["path", "status", "services"]),
        "changed_functions": function(&changes["changed_functions"], cap(200)),
        "untested": function(&changes["untested"], cap(200)),
        "impact": rows(&changes["impact"], cap(50), &["service", "reason", "path"]),
        "edges": edges,
    })
}

/// ccc's security findings: syntax matches with no data flow behind them.
fn security(sast: &Result<Value, String>, cap: &dyn Fn(usize) -> usize) -> Value {
    let sast = match sast {
        Ok(sast) => sast,
        Err(reason) => return json!({ "available": false, "reason": reason }),
    };
    let mut counts: BTreeMap<String, u64> =
        ["high", "medium", "low"].into_iter().map(|s| (s.to_string(), 0)).collect();
    let findings: Vec<Value> = list(&sast["findings"])
        .iter()
        .map(|finding| {
            let severity = finding["severity"].as_str().unwrap_or("low").to_ascii_lowercase();
            *counts.entry(severity.clone()).or_default() += 1;
            let mut kept = pick(
                finding,
                &["rule", "cwe", "file", "line", "function", "message", "evidence", "hint"],
            );
            kept["severity"] = json!(severity);
            kept
        })
        .collect();
    let rank = |finding: &Value| match finding["severity"].as_str() {
        Some("high") => 0,
        Some("medium") => 1,
        _ => 2,
    };
    let mut findings = findings;
    findings.sort_by_key(rank);
    findings.truncate(cap(200));
    json!({
        "available": true,
        "files_scanned": sast["files_scanned"],
        "rules": sast["rules"],
        "counts": counts,
        "total": list(&sast["findings"]).len(),
        "findings": findings,
    })
}

/// The packages resolved from lockfiles, and those with a known advisory.
fn dependencies(audit: &Result<Value, String>, cap: &dyn Fn(usize) -> usize) -> Value {
    let audit = match audit {
        Ok(audit) => audit,
        Err(reason) => return json!({ "available": false, "reason": reason }),
    };
    let packages = list(&audit["packages"]);
    let findings: Vec<Value> = list(&audit["findings"])
        .iter()
        .take(cap(200))
        .map(|finding| {
            let package = &finding["package"];
            let advisory = &finding["advisory"];
            json!({
                "ecosystem": ecosystem(&package["ecosystem"]),
                "name": package["name"],
                "version": package["version"],
                "direct": package["direct"],
                "dev": package["dev"],
                "lockfile": package["lockfile"],
                "id": advisory["id"],
                "aliases": first(&advisory["aliases"], 5),
                "summary": advisory["summary"],
                "severity": advisory["severity"].as_str().map(str::to_ascii_lowercase),
                "fixed": advisory["fixed"],
                "url": advisory["url"],
            })
        })
        .collect();
    let runtime = list(&audit["findings"]).iter().filter(|f| f["package"]["dev"] != true).count();
    json!({
        "available": true,
        "assessed": audit["assessed"],
        "error": audit["error"],
        "lockfiles": audit["lockfiles"],
        "packages": packages.len(),
        "direct": packages.iter().filter(|p| p["direct"] == true).count(),
        "total": list(&audit["findings"]).len(),
        "runtime": runtime,
        "findings": findings,
        "unresolved": rows(&audit["unresolved"], cap(50), &["manifest", "reason"]),
    })
}

/// Every package `ccc audit` resolved from a repository's lockfiles, kept apart from the report
/// because End of life reads it: what the repository runs first, then what it is only built
/// with, as many as fit a record.
pub struct Packages {
    pub listed: Vec<Value>,
    /// How many ccc resolved, which `listed` falls short of only for a very large repository.
    pub total: usize,
}

/// The packages a scan found, or nothing where `ccc audit` did not run, so the ones found before
/// are kept rather than emptied.
pub fn packages(audit: &Result<Value, String>) -> Option<Packages> {
    let audit = audit.as_ref().ok()?;
    let mut listed: Vec<Value> = list(&audit["packages"])
        .iter()
        .map(|package| {
            json!({
                "ecosystem": ecosystem(&package["ecosystem"]),
                "name": package["name"],
                "version": package["version"],
                "direct": package["direct"] == true,
                "dev": package["dev"] == true,
                "lockfile": package["lockfile"],
            })
        })
        .collect();
    let total = listed.len();
    listed.sort_by_key(|package| (package["dev"] == true, package["direct"] != true));
    let mut size = 0;
    listed.retain(|package| {
        size += package.to_string().len() + 1;
        size <= MAX_BYTES
    });
    Some(Packages { listed, total })
}

/// OSV's name for an ecosystem, which is how people know it.
fn ecosystem(value: &Value) -> Value {
    let named = match value.as_str() {
        Some("CratesIo") => "crates.io",
        Some("Npm") => "npm",
        Some("PyPi") => "PyPI",
        Some(other) => other,
        None => "",
    };
    json!(named)
}

/// The figures lists and trends are drawn from, and what changed.
fn summary(report: &Value) -> Value {
    let totals = &report["totals"];
    let mut languages: Vec<&Value> = list(&report["languages"]).iter().collect();
    languages.sort_by_key(|language| std::cmp::Reverse(number(&language["lines"])));
    let by_rule = report["lints"]["findings"].as_array();
    let warnings = by_rule.map_or(0, |f| f.iter().filter(|x| x["severity"] == "warn").count());
    let changes = &report["changes"];
    let security = &report["security"];
    let dependencies = &report["dependencies"];
    json!({
        "files": number(&totals["files"]),
        "lines": number(&totals["lines"]),
        "functions": number(&totals["functions"]),
        "edges": number(&totals["edges"]),
        "languages": languages.iter().take(3).map(|l| pick(l, &["language", "lines"])).collect::<Vec<_>>(),
        "lint_warnings": warnings,
        "lints": list(&report["lints"]["findings"]).len(),
        "cycles": number(&report["hot"]["cycles_total"]),
        "max_complexity": list(&report["complexity"]["functions"]).first().map_or(0, |f| number(&f["complexity"])),
        "untested": number(&report["tests"]["summary"]["untested"]),
        "testable": number(&report["tests"]["summary"]["functions"]),
        "services": list(&report["services"]["services"]).len(),
        "changes": {
            "available": changes["available"],
            "files": number(&changes["counts"]["changed_files"]),
            "functions": number(&changes["counts"]["changed_functions"]),
            "untested": number(&changes["counts"]["untested"]),
        },
        "security": {
            "available": security["available"],
            "high": number(&security["counts"]["high"]),
            "medium": number(&security["counts"]["medium"]),
            "low": number(&security["counts"]["low"]),
        },
        "dependencies": {
            "available": dependencies["available"],
            "assessed": dependencies["assessed"],
            "packages": number(&dependencies["packages"]),
            "advisories": number(&dependencies["total"]),
            "runtime": number(&dependencies["runtime"]),
        },
    })
}

/// A report or summary as someone without the `security` permission may see it.
pub fn without_security(value: &mut Value) {
    if let Some(object) = value.as_object_mut() {
        object.remove("security");
        object.remove("dependencies");
    }
}
