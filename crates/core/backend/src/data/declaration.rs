//! Checking a manifest's `data` (DOC-SPEC §4.3): that it is well formed (`bad-data`), and that it
//! can replace the declaration already in use without breaking the version still serving
//! (`incompatible-data`).

use doc_plugin_protocol::data::{
    Collection, Declaration, Field, FieldType, MAX_COLLECTIONS, MAX_FIELDS, MAX_INDEXES, OnDelete,
    Readers,
};
use doc_plugin_protocol::valid_plugin_id;
use serde_json::Value;

/// The `core.*` collections a `ref` may point at.
const REFERABLE: [&str; 3] = ["core.users", "core.service-accounts", "core.plugins"];

/// The kind of key a reference holds: the target's key, or what a core record is keyed by.
pub fn ref_kind(declaration: &Declaration, to: &str) -> FieldType {
    match to {
        "core.plugins" => FieldType::Text,
        "core.users" | "core.service-accounts" => FieldType::Uuid,
        own => declaration
            .collections
            .get(own)
            .and_then(|target| target.key().and_then(|key| target.fields.get(key)))
            .map_or(FieldType::Text, |key| key.kind),
    }
}

/// The kind a field's values are checked and stored as.
pub fn stored_kind(declaration: &Declaration, field: &Field) -> FieldType {
    match (&field.kind, &field.to) {
        (FieldType::Ref, Some(to)) => ref_kind(declaration, to),
        (kind, _) => *kind,
    }
}

pub fn valid_collection_name(name: &str) -> bool {
    let mut chars = name.chars();
    name.len() <= 32
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

pub fn valid_field_name(name: &str) -> bool {
    let mut chars = name.chars();
    name.len() <= 32
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Why a declaration is malformed, naming the collection and field at fault.
pub fn validate(declaration: &Declaration) -> Result<(), String> {
    if declaration.collections.len() > MAX_COLLECTIONS {
        return Err(format!("a plugin has at most {MAX_COLLECTIONS} collections"));
    }
    for (name, collection) in &declaration.collections {
        if !valid_collection_name(name) {
            return Err(format!("`{name}` is not a collection name"));
        }
        collection_valid(declaration, collection).map_err(|err| format!("{name}: {err}"))?;
    }
    Ok(())
}

fn collection_valid(declaration: &Declaration, collection: &Collection) -> Result<(), String> {
    if collection.fields.is_empty() || collection.fields.len() > MAX_FIELDS {
        return Err(format!("a collection has 1 to {MAX_FIELDS} fields"));
    }
    let keys: Vec<&String> =
        collection.fields.iter().filter(|(_, field)| field.key).map(|(name, _)| name).collect();
    let [key] = keys.as_slice() else {
        return Err("exactly one field is the key".into());
    };
    let key_field = &collection.fields[*key];
    if !matches!(key_field.kind, FieldType::Uuid | FieldType::Text) {
        return Err(format!("the key `{key}` is a uuid or a text field"));
    }
    if key_field.deprecated {
        return Err(format!("the key `{key}` cannot be deprecated"));
    }
    for (field_name, field) in &collection.fields {
        if !valid_field_name(field_name) {
            return Err(format!("`{field_name}` is not a field name"));
        }
        field_valid(declaration, field).map_err(|err| format!("{field_name}: {err}"))?;
    }
    if collection.indexes.len() + collection.unique.len() > MAX_INDEXES {
        return Err(format!(
            "a collection has at most {MAX_INDEXES} indexes and unique constraints"
        ));
    }
    for (what, lists) in [("index", &collection.indexes), ("unique constraint", &collection.unique)]
    {
        for fields in lists {
            if fields.is_empty() {
                return Err(format!("an {what} names at least one field"));
            }
            for field in fields {
                let Some(declared) = collection.fields.get(field) else {
                    return Err(format!("the {what} names `{field}`, which is not a field"));
                };
                if matches!(declared.kind, FieldType::Json | FieldType::Bytes | FieldType::List) {
                    return Err(format!(
                        "`{field}` is {}, which an {what} cannot cover",
                        declared.kind.name()
                    ));
                }
            }
        }
    }
    for field in &collection.search {
        match collection.fields.get(field) {
            Some(declared) if declared.kind == FieldType::Text => {}
            Some(_) => {
                return Err(format!("search covers text fields only, and `{field}` is not one"));
            }
            None => return Err(format!("search names `{field}`, which is not a field")),
        }
    }
    if let Some(export) = &collection.export {
        if let Readers::Plugins(plugins) = &export.read
            && let Some(bad) = plugins.iter().find(|plugin| !valid_plugin_id(plugin))
        {
            return Err(format!("the export names `{bad}`, which is not a plugin ID"));
        }
        for field in export.fields.iter().flatten() {
            if !collection.fields.contains_key(field) {
                return Err(format!("the export names `{field}`, which is not a field"));
            }
        }
    }
    Ok(())
}

fn field_valid(declaration: &Declaration, field: &Field) -> Result<(), String> {
    let kind = field.kind;
    let numeric = matches!(kind, FieldType::Integer | FieldType::Number);
    if field.max.is_some() && !(numeric || matches!(kind, FieldType::Text | FieldType::Bytes)) {
        return Err(format!("`max` does not apply to {}", kind.name()));
    }
    if field.min.is_some() && !numeric {
        return Err(format!("`min` does not apply to {}", kind.name()));
    }
    if let (Some(min), Some(max)) = (field.min, field.max)
        && min > max
    {
        return Err("`min` is above `max`".into());
    }
    if matches!(kind, FieldType::Text | FieldType::Bytes) && field.max.is_some_and(|max| max < 0.0)
    {
        return Err("`max` is not negative".into());
    }
    if !field.one_of.is_empty() && kind != FieldType::Text {
        return Err("`one_of` applies to text only".into());
    }
    match (kind, field.scale) {
        (FieldType::Decimal, Some(scale)) if scale > 30 => {
            return Err("`scale` is at most 30".into());
        }
        (FieldType::Decimal, _) => {}
        (_, Some(_)) => return Err("`scale` applies to decimal only".into()),
        _ => {}
    }
    match (kind, &field.to) {
        (FieldType::Ref, Some(to)) => {
            let own = declaration.collections.get(to.as_str());
            if own.is_none() && !REFERABLE.contains(&to.as_str()) {
                return Err(format!(
                    "`to` names `{to}`, which is neither a collection here nor a core one"
                ));
            }
            if own.is_some_and(|other| other.key().is_none()) {
                return Err(format!("`{to}` has no key to refer to"));
            }
            if field.on_delete.is_some() && own.is_none() {
                return Err("`on_delete` applies to references within the plugin only".into());
            }
            if field.on_delete == Some(OnDelete::Null) && field.required {
                return Err("`on_delete: null` needs a field that can be null".into());
            }
        }
        (FieldType::Ref, None) => return Err("a reference needs `to`".into()),
        (_, Some(_)) => return Err("`to` applies to references only".into()),
        _ => {}
    }
    if field.on_delete.is_some() && kind != FieldType::Ref {
        return Err("`on_delete` applies to references only".into());
    }
    match (kind, field.of) {
        (FieldType::List, None) => return Err("a list needs `of`".into()),
        (FieldType::List, Some(_)) => {}
        (_, Some(_)) => return Err("`of` applies to lists only".into()),
        _ => {}
    }
    if let Some(default) = &field.default {
        if field.key && kind == FieldType::Text {
            return Err("a text key has no default; it is always given".into());
        }
        let special = matches!((kind, default), (FieldType::Timestamp, Value::String(s)) if s == "now")
            || matches!((kind, default), (FieldType::Uuid, Value::String(s)) if s == "uuid");
        if !special {
            let kind = stored_kind(declaration, field);
            let checked = crate::data::values::check(field, kind, default);
            checked.map_err(|err| format!("the default {err}"))?;
        }
    }
    Ok(())
}

/// A field whose declaration, and so whose stored values, the two versions agree on.
fn field_compatible(old: &Field, new: &Field) -> Result<(), String> {
    if old.kind != new.kind {
        return Err(format!("its type changed from {} to {}", old.kind.name(), new.kind.name()));
    }
    if old.key != new.key {
        return Err("the key changed".into());
    }
    if old.of != new.of || old.to != new.to {
        return Err("what it holds changed".into());
    }
    if old.scale != new.scale {
        return Err("its scale changed".into());
    }
    if old.on_delete.unwrap_or(OnDelete::Restrict) != new.on_delete.unwrap_or(OnDelete::Restrict) {
        return Err("its `on_delete` changed".into());
    }
    if !old.required && new.required {
        return Err("it became required".into());
    }
    match (old.max, new.max) {
        (None, Some(_)) => return Err("it gained a `max`".into()),
        (Some(old), Some(new)) if new < old => return Err("its `max` went down".into()),
        _ => {}
    }
    match (old.min, new.min) {
        (None, Some(_)) => return Err("it gained a `min`".into()),
        (Some(old), Some(new)) if new > old => return Err("its `min` went up".into()),
        _ => {}
    }
    if old.one_of.is_empty() && !new.one_of.is_empty() {
        return Err("it gained a `one_of`".into());
    }
    // No `one_of` at all takes any text, which is looser than any list.
    if !new.one_of.is_empty()
        && let Some(dropped) = old.one_of.iter().find(|value| !new.one_of.contains(value))
    {
        return Err(format!("`{dropped}` left its `one_of`"));
    }
    Ok(())
}

/// Whether `new` can take over from `old`: only additions, relaxations and deprecated removals.
pub fn compatible(old: &Declaration, new: &Declaration) -> Result<(), String> {
    for (name, before) in &old.collections {
        let Some(after) = new.collections.get(name) else {
            if before.deprecated {
                continue;
            }
            return Err(format!("{name} was removed without being deprecated first"));
        };
        for (field_name, field) in &before.fields {
            match after.fields.get(field_name) {
                Some(now) => field_compatible(field, now)
                    .map_err(|err| format!("{name}.{field_name}: {err}"))?,
                None if field.deprecated => {}
                None => {
                    return Err(format!(
                        "{name}.{field_name} was removed without being deprecated first"
                    ));
                }
            }
        }
        for (field_name, field) in &after.fields {
            if !before.fields.contains_key(field_name) && field.required && field.default.is_none()
            {
                return Err(format!(
                    "{name}.{field_name} is new and required, so it needs a default for the records already there"
                ));
            }
            if !before.fields.contains_key(field_name) && field.key {
                return Err(format!("{name}: the key changed to {field_name}"));
            }
        }
    }
    Ok(())
}

/// What storage holds for both versions in a handover: `new` wins, and what only `old` has is optional.
pub fn merged(old: &Declaration, new: &Declaration) -> Declaration {
    let mut merged = new.clone();
    for (name, before) in &old.collections {
        let Some(after) = merged.collections.get_mut(name) else {
            merged.collections.insert(name.clone(), before.clone());
            continue;
        };
        for (field_name, field) in &before.fields {
            if !after.fields.contains_key(field_name) {
                let optional = Field { required: false, ..field.clone() };
                after.fields.insert(field_name.clone(), optional);
            }
        }
        for (lists, old_lists) in
            [(&mut after.indexes, &before.indexes), (&mut after.unique, &before.unique)]
        {
            for fields in old_lists {
                if !lists.contains(fields) {
                    lists.push(fields.clone());
                }
            }
        }
    }
    merged
}
