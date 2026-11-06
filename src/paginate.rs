//! Auto-pagination driven by `paginators-1.json` (botocore `paginate.py`).
//!
//! Pages are handled as JSON (after output conversion), which is what both
//! the result aggregation and JMESPath token expressions operate on.

use crate::value::Value;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::{Map, Value as J};

#[derive(Debug, Clone)]
pub struct PaginatorConfig {
    pub input_tokens: Vec<String>,
    pub output_tokens: Vec<String>,
    pub limit_key: Option<String>,
    pub result_keys: Vec<String>,
    pub more_results: Option<String>,
    pub non_aggregate_keys: Vec<String>,
}

fn str_or_list(v: Option<&J>) -> Vec<String> {
    match v {
        Some(J::String(s)) => vec![s.clone()],
        Some(J::Array(a)) => a
            .iter()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

impl PaginatorConfig {
    pub fn from_json(v: &J) -> Option<Self> {
        let input_tokens = str_or_list(v.get("input_token"));
        let output_tokens = str_or_list(v.get("output_token"));
        let result_keys = str_or_list(v.get("result_key"));
        if input_tokens.is_empty() || result_keys.is_empty() {
            return None;
        }
        Some(PaginatorConfig {
            input_tokens,
            output_tokens,
            limit_key: v.get("limit_key").and_then(J::as_str).map(str::to_string),
            result_keys,
            more_results: v
                .get("more_results")
                .and_then(J::as_str)
                .map(str::to_string),
            non_aggregate_keys: str_or_list(v.get("non_aggregate_keys")),
        })
    }

    /// Look up the paginator for an operation in a `paginators-1.json` document.
    pub fn for_operation(doc: &J, operation: &str) -> Option<Self> {
        doc.get("pagination")
            .and_then(|p| p.get(operation))
            .and_then(Self::from_json)
    }
}

#[derive(Debug, Clone, Default)]
pub struct PaginationArgs {
    pub max_items: Option<usize>,
    pub page_size: Option<i64>,
    pub starting_token: Option<String>,
}

fn search(expr: &str, data: &J) -> J {
    let Ok(compiled) = jmespath::compile(expr) else {
        return J::Null;
    };
    let Ok(var) = jmespath::Variable::from_serializable(data) else {
        return J::Null;
    };
    match compiled.search(var) {
        Ok(result) => serde_json::to_value(&*result).unwrap_or(J::Null),
        Err(_) => J::Null,
    }
}

/// `set_value_from_jmespath` for dotted paths.
fn set_path(target: &mut Map<String, J>, path: &str, value: J) {
    let mut parts: Vec<&str> = path.split('.').collect();
    let last = parts.pop().unwrap_or(path);
    let mut cur = target;
    for p in parts {
        let entry = cur
            .entry(p.to_string())
            .or_insert_with(|| J::Object(Map::new()));
        if !entry.is_object() {
            *entry = J::Object(Map::new());
        }
        cur = entry.as_object_mut().unwrap();
    }
    cur.insert(last.to_string(), value);
}

pub fn encode_token(token: &Map<String, J>) -> String {
    B64.encode(J::Object(token.clone()).to_string())
}

/// Returns the decoded service tokens and the number of items to skip.
pub fn decode_token(raw: &str, cfg: &PaginatorConfig) -> (Map<String, J>, usize) {
    if let Ok(bytes) = B64.decode(raw.trim())
        && let Ok(J::Object(mut m)) = serde_json::from_slice::<J>(&bytes)
    {
        let skip = m
            .remove("boto_truncate_amount")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        return (m, skip);
    }
    // Old style `token1___token2___amount`, or a raw service token.
    let mut parts: Vec<&str> = raw.split("___").collect();
    let mut skip = 0;
    if parts.len() == cfg.input_tokens.len() + 1
        && let Ok(n) = parts.last().unwrap().parse()
    {
        skip = n;
        parts.pop();
    }
    let mut m = Map::new();
    for (k, v) in cfg.input_tokens.iter().zip(parts) {
        if v != "None" {
            m.insert(k.clone(), J::String(v.to_string()));
        }
    }
    (m, skip)
}

/// Fetch every page and build the aggregated result (`build_full_result`).
pub fn paginate<E>(
    cfg: &PaginatorConfig,
    base_params: &Value,
    args: &PaginationArgs,
    mut call: impl FnMut(&Value) -> Result<J, E>,
) -> Result<J, E> {
    let mut params = match base_params {
        Value::Map(m) => m.clone(),
        _ => Default::default(),
    };
    if let (Some(limit), Some(size)) = (&cfg.limit_key, args.page_size) {
        params.insert(limit.clone(), Value::Int(size));
    }
    let mut next_token: Map<String, J> = Map::new();
    let mut starting_skip = 0;
    if let Some(raw) = &args.starting_token {
        let (tokens, skip) = decode_token(raw, cfg);
        for (k, v) in &tokens {
            params.insert(k.clone(), Value::from_json(v));
        }
        next_token = tokens;
        starting_skip = skip;
    }

    let primary = cfg.result_keys[0].clone();
    let mut complete = Map::new();
    let mut non_aggregate = Map::new();
    let mut resume: Option<Map<String, J>> = None;
    let mut total = 0usize;
    let mut previous: Option<Map<String, J>> = None;
    let mut first = true;
    loop {
        let mut page = call(&Value::Map(params.clone()))?;
        if first {
            if starting_skip > 0
                && let J::Object(obj) = &mut page
            {
                let items = search(&primary, &J::Object(obj.clone()));
                let rest = items
                    .as_array()
                    .map(|a| a.iter().skip(starting_skip).cloned().collect())
                    .unwrap_or_default();
                set_path(obj, &primary, J::Array(rest));
            }
            for key in &cfg.non_aggregate_keys {
                set_path(&mut non_aggregate, key, search(key, &page));
            }
            first = false;
        } else {
            starting_skip = 0;
        }

        let current = search(&primary, &page);
        let count = current.as_array().map_or(0, Vec::len);
        let truncate = args
            .max_items
            .map(|m| (total + count) as i64 - m as i64)
            .unwrap_or(0);
        if truncate > 0 {
            let keep = count - truncate as usize;
            if let J::Object(obj) = &mut page {
                let kept = current
                    .as_array()
                    .map(|a| a[..keep].to_vec())
                    .unwrap_or_default();
                set_path(obj, &primary, J::Array(kept));
            }
            let mut token = next_token.clone();
            token.insert("boto_truncate_amount".into(), J::from(keep + starting_skip));
            resume = Some(token);
            merge_page(&mut complete, &page, &cfg.result_keys);
            break;
        }
        merge_page(&mut complete, &page, &cfg.result_keys);
        total += count;

        next_token = next_tokens(cfg, &page);
        if next_token.is_empty() {
            break;
        }
        if args.max_items == Some(total) {
            resume = Some(next_token);
            break;
        }
        if previous.as_ref() == Some(&next_token) {
            break;
        }
        for k in &cfg.input_tokens {
            match next_token.get(k) {
                Some(v) => params.insert(k.clone(), Value::from_json(v)),
                None => params.shift_remove(k),
            };
        }
        previous = Some(next_token.clone());
    }

    for (k, v) in non_aggregate {
        complete.entry(k).or_insert(v);
    }
    if let Some(token) = resume {
        complete.insert("NextToken".into(), J::String(encode_token(&token)));
    }
    Ok(J::Object(complete))
}

fn next_tokens(cfg: &PaginatorConfig, page: &J) -> Map<String, J> {
    if let Some(more) = &cfg.more_results {
        let v = search(more, page);
        if v.is_null() || v == J::Bool(false) {
            return Map::new();
        }
    }
    let mut out = Map::new();
    for (expr, key) in cfg.output_tokens.iter().zip(&cfg.input_tokens) {
        let v = search(expr, page);
        let present = match &v {
            J::Null => false,
            J::String(s) => !s.is_empty(),
            J::Bool(b) => *b,
            _ => true,
        };
        if present {
            out.insert(key.clone(), v);
        }
    }
    out
}

fn merge_page(complete: &mut Map<String, J>, page: &J, result_keys: &[String]) {
    for key in result_keys {
        let value = search(key, page);
        if value.is_null() {
            continue;
        }
        let existing = search(key, &J::Object(complete.clone()));
        let merged = match (existing, value) {
            (J::Null, v) => v,
            (J::Array(mut a), J::Array(b)) => {
                a.extend(b);
                J::Array(a)
            }
            (J::String(a), J::String(b)) => J::String(a + &b),
            (J::Number(a), J::Number(b)) => match (a.as_i64(), b.as_i64()) {
                (Some(x), Some(y)) => J::from(x + y),
                _ => J::from(a.as_f64().unwrap_or(0.0) + b.as_f64().unwrap_or(0.0)),
            },
            (existing, _) => existing,
        };
        set_path(complete, key, merged);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg() -> PaginatorConfig {
        PaginatorConfig::from_json(&json!({
            "input_token": "NextToken", "output_token": "NextToken",
            "limit_key": "MaxResults", "result_key": "Items",
            "non_aggregate_keys": ["Owner"]
        }))
        .unwrap()
    }

    fn pages() -> Vec<J> {
        vec![
            json!({"Items": [1, 2], "NextToken": "t1", "Owner": "me"}),
            json!({"Items": [3, 4], "NextToken": "t2", "Owner": "ignored"}),
            json!({"Items": [5]}),
        ]
    }

    fn run(args: PaginationArgs) -> (J, Vec<Value>) {
        let mut seen = Vec::new();
        let pages = pages();
        let out = paginate::<()>(&cfg(), &Value::Map(Default::default()), &args, |p| {
            seen.push(p.clone());
            let idx = match p.get("NextToken").and_then(Value::as_str) {
                None => 0,
                Some("t1") => 1,
                Some(_) => 2,
            };
            Ok(pages[idx].clone())
        })
        .unwrap();
        (out, seen)
    }

    #[test]
    fn aggregates_all_pages() {
        let (out, seen) = run(PaginationArgs {
            page_size: Some(2),
            ..Default::default()
        });
        assert_eq!(out, json!({"Items": [1, 2, 3, 4, 5], "Owner": "me"}));
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[0].get("MaxResults"), Some(&Value::Int(2)));
    }

    #[test]
    fn max_items_and_resume() {
        let (out, _) = run(PaginationArgs {
            max_items: Some(3),
            ..Default::default()
        });
        assert_eq!(out["Items"], json!([1, 2, 3]));
        let token = out["NextToken"].as_str().unwrap().to_string();
        let (decoded, skip) = decode_token(&token, &cfg());
        assert_eq!((decoded["NextToken"].as_str(), skip), (Some("t1"), 1));

        let (rest, _) = run(PaginationArgs {
            starting_token: Some(token),
            ..Default::default()
        });
        assert_eq!(rest["Items"], json!([4, 5]));
    }
}
