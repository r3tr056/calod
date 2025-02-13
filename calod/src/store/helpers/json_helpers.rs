use serde_json::{Value as JsonValue, json, Number as JsonNumber, Map as JsonMap};
use std::cmp;

use crate::store::calod_store::CacheError;


pub fn json_type_to_string(json_value: &JsonValue) -> String {
    match json_value {
        JsonValue::Null => "null".to_string(),
        JsonValue::Bool(_) => "boolean".to_string(),
        JsonValue::Number(_) => "number".to_string(),
        JsonValue::String(_) => "string".to_string(),
        JsonValue::Array(_) => "array".to_string(),
        JsonValue::Object(_) => "object".to_string(), 
    }
}

#[derive(Debug, Clone)]
enum PathSegment {
    Key(String),
    Index(usize),
}

// Parses a JSONPath string using a limited syntax (eg. "$.foo.bar[0].baz")
// This parser supports
//   - An optional "$" at the beginning
//   - Dot-separated keys
//   - Bracket notation for array indexes (only non-negative integers)
fn parse_json_path(path: &str) -> Result<Vec<PathSegment>, CacheError> {
    let mut segments = Vec::new();
    let mut s = path.trim();

    // remove optional "$" root indicator
    if s.starts_with('$') {
        s = &s[1..];
    }

    // remove loading dot if present.
    if s.starts_with('.') {
        s = &s[1..];
    }

    while !s.is_empty() {
        let mut key = String::new();
        let mut i = 0;
        for c in s.chars() {
            if c == '.' || c == '[' {
                break;
            }
            key.push(c);
            i += c.len_utf8();
        }

        if !key.is_empty() {
            segments.push(PathSegment::Key(key));
        }
        s = &s[i..];

        // if the next chanracter is '[' parse the index
        if s.starts_with('[') {
            s = &s[1..];
            let mut idx_str = String::new();
            for c in s.chars() {
                if c == ']' { break; }
                idx_str.push(c);
            }

            if !s.starts_with(&idx_str) || !s[idx_str.len()..].starts_with(']') {
                return Err(CacheError::InvalidCommandArguments("Missing closing bracket in JSONPath".to_string()));
            }

            s = &s[idx_str.len() + 1..];
            let idx = idx_str.parse::<usize>().map_err(|_| {
                CacheError::InvalidCommandArguments(format!("Invalid array index: {}", idx_str))
            })?;
            segments.push(PathSegment::Index(idx));
        }

        if s.starts_with('.') {
            s = &s[1..];
        }
    }

    Ok(segments)
}

// Traverses (read-only) the JSON document according to the path segments.
// Returns a reference to the target value if found.
fn traverse_path<'a>(doc: &'a JsonValue, segments: &[PathSegment]) -> Option<&'a JsonValue> {
    let mut current = doc;
    for seg in segments {
        match seg {
            PathSegment::Key(ref key) => {
                if let JsonValue::Object(map) = current {
                    current = map.get(key)?;
                }
            }
            PathSegment::Index(idx) => {
                if let JsonValue::Array(arr) = current {
                    current = arr.get(*idx)?;
                } else {
                    return None;
                }
            },
        }
    }
    Some(current)
}

fn traverse_path_mut<'a>(doc: &'a mut JsonValue, segments: &[PathSegment]) -> Result<&'a mut JsonValue, CacheError> {
    if segments.is_empty() {
        return Ok(doc);
    }

    let mut current = doc;
    for i in 0..(segments.len() - 1) {
        let seg = &segments[i];
        let next_seg = &segments[i + 1];
        match seg {
            PathSegment::Key(ref key) => {
                if !current.is_object() {
                    if current.is_null() {
                        *current = JsonValue::Object(JsonMap::new());
                    } else {
                        return Err(CacheError::DataTypeMismatch(key.clone(), "object".to_string(), json_type_to_string(current)));
                    }
                }

                let obj = current.as_object_mut().unwrap();
                if !obj.contains_key(key) {
                    let new_val = match next_seg {
                        PathSegment::Key(_) => JsonValue::Object(JsonMap::new()),
                        PathSegment::Index(_) => JsonValue::Array(vec![]),
                    };
                    obj.insert(key.clone(), new_val);
                }
                current = obj.get_mut(key).unwrap();
            }
            PathSegment::Index(idx) => {
                if !current.is_array() {
                    if current.is_null() {
                        *current = JsonValue::Array(vec![]);
                    } else {
                        return Err(CacheError::DataTypeMismatch(
                            idx.to_string(),
                            "array".to_string(),
                            json_type_to_string(current),
                        ));
                    }
                }
                let arr = current.as_array_mut().unwrap();
                // Extend array if necessary.
                while arr.len() <= *idx {
                    // Insert Null values.
                    arr.push(JsonValue::Null);
                }
                current = &mut arr[*idx];
            },
        }
    }

    Ok(current)
}

// for operations that modify a sub-value
fn get_parent_mut<'a>(doc: &'a mut JsonValue, segments: &[PathSegment]) -> Result<(&'a mut JsonValue, PathSegment), CacheError> {
    if segments.is_empty() {
        return Err(CacheError::InvalidCommandArguments("Cannot operate on root element".to_string()));
    }
    let parent_segments = &segments[..segments.len() - 1];
    let last_segment = segments.last().unwrap().clone();
    let parent = traverse_path_mut(doc, parent_segments)?;
    Ok((parent, last_segment))
}

// Sets the value at the given JSONPath, creating any missing intermediate nodes.
// If the path is empty, replaces the entire document.
pub fn jsonpath_set(doc: &mut JsonValue, path: &str, value: JsonValue) -> Result<JsonValue, CacheError> {
    let segments = parse_json_path(path)?;
    if segments.is_empty() {
        *doc = value;
        return Ok(doc.clone());
    }

    let (parent, last_seg) = get_parent_mut(doc, &segments)?;
    match last_seg {
        PathSegment::Index(idx) => {
            if !parent.is_array() {
                return Err(CacheError::DataTypeMismatch(idx.to_string(), "array".to_string(), json_type_to_string(&parent)));
            }
            let arr = parent.as_array_mut().unwrap();
            if idx < arr.len() {
                arr[idx] = value;
            } else if idx == arr.len() {
                arr.push(value);
            } else {
                return Err(CacheError::InvalidCommandArguments(format!("Index {} out of bounds", idx)));
            }
        }
        PathSegment::Key(key) => {
            if !parent.is_object() {
                return Err(CacheError::DataTypeMismatch(
                    key,
                    "object".to_string(),
                    json_type_to_string(parent),
                ));
            }
            parent.as_object_mut().unwrap().insert(key, value);
        }
    }

    Ok(traverse_path(doc, &segments).unwrap().clone())
}

// Gets the value at the given JSONPath. Returns Null if not found.
pub fn jsonpath_get(doc: &JsonValue, path: &str) -> Result<JsonValue, CacheError> {
    let segments = parse_json_path(path)?;
    if let Some(val) = traverse_path(doc, &segments) {
        Ok(val.clone())
    } else {
        Ok(JsonValue::Null)
    }
}

// Deletes the value at the given JSONPath. For object keys, removes the key
// for array indexes, removes the element (shifting subsequent elements.)
// Returns the number of elements deleted (0 or 1)
pub fn jsonpath_del(doc: &mut JsonValue, path: &str) -> Result<usize, CacheError> {
    let segments = parse_json_path(path)?;
    let (parent, last_seg) = get_parent_mut(doc, &segments)?;
    match last_seg {
        PathSegment::Key(key) => {
            if let JsonValue::Object(map) = parent {
                Ok(map.remove(&key).map(|_| 1).unwrap_or(0))
            } else {
                Err(CacheError::DataTypeMismatch(key, "object".to_string(), json_type_to_string(parent)))
            }
        },
        PathSegment::Index(idx) => {
            if let JsonValue::Array(arr) = parent {
                if idx < arr.len() {
                    arr.remove(idx);
                    Ok(1)
                } else {
                    Ok(0)
                }
            } else {
                Err(CacheError::DataTypeMismatch(idx.to_string(), "array".to_string(), json_type_to_string(parent)))
            }
        },
    }
}

// Increments the number stored at the JSONPath by a given increment.
// If the current value is an integer, the result is truncated to an integer
pub fn jsonpath_numincrby(doc: &mut JsonValue, path: &str, increment: f64) -> Result<JsonValue, CacheError> {
    let segments = parse_json_path(path)?;
    let current_val = traverse_path(doc, &segments)
        .ok_or_else(|| {
            CacheError::InvalidCommandArguments(format!("JSONPath not found: {}", path))
        })?
        .clone();

    if !current_val.is_number() {
        return Err(CacheError::InvalidCommandArguments(format!("Value at the path is not a number: {}", path)));
    }
    
    let new_val = if let Some(n) = current_val.as_f64() {
        let updated = n + increment;
        if current_val.is_i64() || current_val.is_u64() {
            JsonValue::Number(JsonNumber::from(updated.trunc() as i64))
        } else {
            JsonNumber::from_f64(updated).map(JsonValue::Number).ok_or_else(|| { CacheError::InternalError("Failed to convert incremented value".to_string()) })?
        }
    } else {
        return Err(CacheError::InvalidCommandArguments(format!(
            "Value at path cannot be converted to number: {}",
            path
        )));
    };

    jsonpath_set(doc, path, new_val.clone())?;
    Ok(new_val)
}

// Appends the given string to the string at the JSONPath
pub fn jsonpath_strappend(doc: &mut JsonValue, path: &str, value_to_append: &str) -> Result<JsonValue, CacheError> {
    let segments = parse_json_path(path)?;
    let current_val = traverse_path(doc, &segments).ok_or_else(|| {
        CacheError::InvalidCommandArguments(format!("JSONPath not found: {}", path))
    })?;

    if !current_val.is_string() {
        return Err(CacheError::InvalidCommandArguments(format!("Value at path is not a string for STRAPPEND: {}", path)));
    }

    let mut new_str = current_val.as_str().unwrap().to_owned();
    new_str.push_str(value_to_append);
    jsonpath_set(doc, path, json!(new_str))
}

// Appends the given values to the array to the JSONPath. If the current value is
// null, it is treated as an empty array
pub fn jsonpath_arrappend(doc: &mut JsonValue, path: &str, values_to_append: &Vec<JsonValue>) -> Result<JsonValue, CacheError> {
    let segments = parse_json_path(path)?;
    let current_val = traverse_path(doc, &segments).cloned().unwrap_or(JsonValue::Null);
    let mut new_arr = if current_val.is_array() {
        current_val.as_array().unwrap().clone()
    } else if current_val.is_null() {
        vec![]
    } else {
        return Err(CacheError::DataTypeMismatch(path.to_string(), "Array or Null".to_string(), json_type_to_string(&current_val)));
    };

    new_arr.extend(values_to_append.clone());
    jsonpath_set(doc, path, json!(new_arr))
}

// Sets the given key to a value in the object at the JSONPath
// If the current value is null, it is treated as an empty object
pub fn jsonpath_objset(doc: &mut JsonValue, path: &str, key_to_set: &str, value_to_set: JsonValue) -> Result<JsonValue, CacheError> {
    let segments = parse_json_path(path)?;
    let current_val = traverse_path(doc, &segments).cloned().unwrap_or(JsonValue::Null);
    let mut new_obj = if current_val.is_object() {
        current_val.as_object().unwrap().clone()
    } else if current_val.is_null() {
        JsonMap::new()
    } else {
        return Err(CacheError::DataTypeMismatch(path.to_string(), "Object or Null".to_string(), json_type_to_string(&current_val)));
    };

    new_obj.insert(key_to_set.to_string(), value_to_set);
    jsonpath_set(doc, path, json!(new_obj))
}

// Returns the keys of the object at the JSONPath
pub fn jsonpath_objkeys(doc: &JsonValue, path: &str) -> Result<Vec<String>, CacheError> {
    let segments = parse_json_path(path)?;
    let current_val = traverse_path(doc, &segments).ok_or_else(|| {
        CacheError::InvalidCommandArguments(format!("JSONPath not found: {}", path))
    })?;

    if let JsonValue::Object(map) = current_val {
        Ok(map.keys().cloned().collect())
    } else {
        Err(CacheError::DataTypeMismatch(path.to_string(), "Object".to_string(), json_type_to_string(current_val)))
    }
}

// Returns the number of keys in the object at the JSONPath
pub fn jsonpath_objlen(doc: &JsonValue, path: &str) -> Result<usize, CacheError> {
    let segments = parse_json_path(path)?;
    let current_val = traverse_path(doc, &segments).ok_or_else(|| {
        CacheError::InvalidCommandArguments(format!("JSONPath not found: {}", path))
    })?;

    if let JsonValue::Object(map) = current_val {
        Ok(map.len())
    } else {
        Err(CacheError::DataTypeMismatch(path.to_string(), "Object".to_string(), json_type_to_string(current_val)))
    }
}

// Searches for a value in the array at the JSONPath within an optional index range.
// Returns the index if found, or -1 if not found.
pub fn jsonpath_arrindex(doc: &JsonValue, path: &str, value_to_find: &JsonValue, range: Option<(isize, isize)>) -> Result<isize, CacheError> {
    let segments = parse_json_path(path)?;
    let current_val = traverse_path(doc, &segments).ok_or_else(|| {
        CacheError::InvalidCommandArguments(format!("JSONPath not found: {}", path))
    })?;

    if let JsonValue::Array(arr) = current_val {
        let len = arr.len() as isize;
        let (mut start, mut stop) = range.unwrap_or((0, len - 1));
        // adjust negative indices
        if start < 0 {
            start = cmp::max(start + len, 0);
        }
        if stop < 0 {
            stop = cmp::max(stop + len, 0);
        }

        // clamp within array bounds
        start = cmp::min(cmp::max(start, 0), len - 1);
        stop = cmp::min(cmp::max(stop, 0), len - 1);
        for (i, item) in arr.iter().enumerate() {
            let i = i as isize;
            if i >= start && i <= stop && item == value_to_find {
                return Ok(i);
            }
        }
        Ok(-1)
    } else {
        Err(CacheError::DataTypeMismatch(path.to_string(), "Array".to_string(), json_type_to_string(current_val)))
    }
}


// Inserts the given values into the array at the JSONPath at the given index.
// Negative indexes count from the end.
pub fn jsonpath_arrinsert(
    doc: &mut JsonValue,
    path: &str,
    index: isize,
    values_to_insert: &Vec<JsonValue>,
) -> Result<JsonValue, CacheError> {
    let segments = parse_json_path(path)?;
    let current_val = traverse_path(doc, &segments).cloned().ok_or_else(|| {
        CacheError::InvalidCommandArguments(format!("JSONPath not found: {}", path))
    })?;
    if let JsonValue::Array(mut arr) = current_val {
        let len = arr.len() as isize;
        let mut insert_pos = if index < 0 {
            index + len
        } else {
            index
        };
        insert_pos = cmp::max(0, cmp::min(insert_pos, len));
        // Insert values one by one.
        for (offset, val) in values_to_insert.iter().cloned().enumerate() {
            arr.insert((insert_pos as usize) + offset, val);
        }
        jsonpath_set(doc, path, json!(arr))
    } else {
        Err(CacheError::DataTypeMismatch(
            path.to_string(),
            "Array".to_string(),
            json_type_to_string(&current_val),
        ))
    }
}

/// Returns the length of the array at the JSONPath.
pub fn jsonpath_arrlen(doc: &JsonValue, path: &str) -> Result<usize, CacheError> {
    let segments = parse_json_path(path)?;
    let current_val = traverse_path(doc, &segments).ok_or_else(|| {
        CacheError::InvalidCommandArguments(format!("JSONPath not found: {}", path))
    })?;
    if let JsonValue::Array(arr) = current_val {
        Ok(arr.len())
    } else {
        Err(CacheError::DataTypeMismatch(
            path.to_string(),
            "Array".to_string(),
            json_type_to_string(current_val),
        ))
    }
}

/// Removes and returns an element from the array at the JSONPath at the given index.
/// If the index is out of bounds, returns Null.
pub fn jsonpath_arrpop(
    doc: &mut JsonValue,
    path: &str,
    index: Option<isize>,
) -> Result<JsonValue, CacheError> {
    let segments = parse_json_path(path)?;
    let current_val = traverse_path(doc, &segments).cloned().ok_or_else(|| {
        CacheError::InvalidCommandArguments(format!("JSONPath not found: {}", path))
    })?;
    if let JsonValue::Array(mut arr) = current_val {
        let len = arr.len() as isize;
        let idx = index.unwrap_or(-1);
        let pop_index = if idx < 0 { idx + len } else { idx };
        if pop_index < 0 || pop_index >= len {
            return Ok(JsonValue::Null);
        }
        let popped = arr.remove(pop_index as usize);
        jsonpath_set(doc, path, json!(arr))?;
        Ok(popped)
    } else {
        Err(CacheError::DataTypeMismatch(
            path.to_string(),
            "Array".to_string(),
            json_type_to_string(&current_val),
        ))
    }
}

/// Trims the array at the JSONPath so that only the elements between the
/// start and stop indexes (inclusive) remain. Returns the new array length.
pub fn jsonpath_arrtrim(
    doc: &mut JsonValue,
    path: &str,
    start: isize,
    stop: isize,
) -> Result<usize, CacheError> {
    let segments = parse_json_path(path)?;
    let current_val = traverse_path(doc, &segments).cloned().ok_or_else(|| {
        CacheError::InvalidCommandArguments(format!("JSONPath not found: {}", path))
    })?;
    if let JsonValue::Array(arr) = current_val {
        let len = arr.len() as isize;
        let mut s = if start < 0 { start + len } else { start };
        let mut e = if stop < 0 { stop + len } else { stop };
        s = cmp::max(s, 0);
        e = cmp::min(e, len - 1);
        let new_arr = if s <= e {
            arr[s as usize..=e as usize].to_vec()
        } else {
            vec![]
        };
        jsonpath_set(doc, path, json!(new_arr.clone()))?;
        Ok(new_arr.len())
    } else {
        Err(CacheError::DataTypeMismatch(
            path.to_string(),
            "Array".to_string(),
            json_type_to_string(&current_val),
        ))
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_jsonpath_set_and_get() {
        let mut doc = json!({});
        let _ = jsonpath_set(&mut doc, "$.a.b", json!(123)).unwrap();
        assert_eq!(jsonpath_get(&doc, "$.a.b").unwrap(), json!(123));
    }

    #[test]
    fn test_jsonpath_arrappend() {
        let mut doc = json!({ "arr": [1, 2] });
        let _ = jsonpath_arrappend(&mut doc, "$.arr", &vec![json!(3), json!(4)]).unwrap();
        assert_eq!(jsonpath_get(&doc, "$.arr").unwrap(), json!([1, 2, 3, 4]));
    }

    #[test]
    fn test_jsonpath_numincrby() {
        let mut doc = json!({ "num": 10 });
        let new_val = jsonpath_numincrby(&mut doc, "$.num", 5.5).unwrap();
        // Since original was an integer, result is truncated.
        assert_eq!(new_val, json!(15));
    }
}