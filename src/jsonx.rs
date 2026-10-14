//! Small helpers for navigating `serde_json::Value` the way the original Go
//! code does with `mapString` / `anyList` / `nestedMap`.

pub fn jstr(v: &serde_json::Value, keys: &[&str]) -> String {
    for key in keys {
        if let Some(s) = v.get(*key).and_then(|x| x.as_str()) {
            let s = s.trim();
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    String::new()
}

pub fn jint(v: &serde_json::Value, keys: &[&str]) -> Option<i64> {
    for key in keys {
        let node = v.get(*key)?;
        if let Some(n) = node.as_i64() {
            return Some(n);
        }
        if let Some(s) = node.as_str() {
            if let Ok(n) = s.trim().parse::<i64>() {
                return Some(n);
            }
        }
    }
    None
}

pub fn jnum_str(v: &serde_json::Value, keys: &[&str]) -> String {
    for key in keys {
        match v.get(*key) {
            Some(serde_json::Value::String(s)) if !s.trim().is_empty() => return s.clone(),
            Some(n @ (serde_json::Value::Number(_) | serde_json::Value::Bool(_))) => {
                return n.to_string().trim_matches('"').to_string();
            }
            _ => {}
        }
    }
    String::new()
}

pub fn jmap<'a>(v: &'a serde_json::Value, keys: &[&str]) -> &'a serde_json::Value {
    const NULL: &serde_json::Value = &serde_json::Value::Null;
    let mut cur = v;
    for key in keys {
        cur = cur.get(*key).unwrap_or(NULL);
    }
    cur
}

/// `anyList`: lists, or objects with a list/items/data child.
pub fn jlist(v: &serde_json::Value) -> Vec<&serde_json::Value> {
    match v {
        serde_json::Value::Array(items) => items.iter().collect(),
        serde_json::Value::Object(_) => {
            for key in ["list", "items", "data"] {
                let out = jlist(v.get(key).unwrap_or(&serde_json::Value::Null));
                if !out.is_empty() {
                    return out;
                }
            }
            Vec::new()
        }
        _ => Vec::new(),
    }
}

pub fn jbool(v: &serde_json::Value, key: &str) -> Option<bool> {
    v.get(key).and_then(|x| x.as_bool())
}
