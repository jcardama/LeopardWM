use super::Config;
use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use std::collections::BTreeMap;
use std::fmt;
use std::ops::Range;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Part {
    Key(String),
    Index(usize),
}

#[derive(Debug)]
enum Node {
    Table(BTreeMap<String, toml::Spanned<Node>>),
    Array(Vec<toml::Spanned<Node>>),
    Value,
}

impl<'de> Deserialize<'de> for Node {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct NodeVisitor;
        impl<'de> Visitor<'de> for NodeVisitor {
            type Value = Node;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a TOML value")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Node, A::Error> {
                let mut result = BTreeMap::new();
                while let Some(key) = map.next_key::<String>()? {
                    if key == "$__toml_private_datetime" {
                        let _: String = map.next_value()?;
                        return Ok(Node::Value);
                    }
                    result.insert(key, map.next_value()?);
                }
                Ok(Node::Table(result))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Node, A::Error> {
                let mut result = Vec::new();
                while let Some(value) = seq.next_element()? {
                    result.push(value);
                }
                Ok(Node::Array(result))
            }
            fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Node, E> {
                Ok(Node::Value)
            }
            fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Node, E> {
                Ok(Node::Value)
            }
            fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<Node, E> {
                Ok(Node::Value)
            }
            fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Node, E> {
                Ok(Node::Value)
            }
            fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<Node, E> {
                Ok(Node::Value)
            }
            fn visit_string<E: serde::de::Error>(self, _: String) -> Result<Node, E> {
                Ok(Node::Value)
            }
        }
        deserializer.deserialize_any(NodeVisitor)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct DroppedField {
    pub path: String,
    pub location: Option<(usize, usize)>,
    pub reason: String,
}

fn locate<'a>(
    node: &'a toml::Spanned<Node>,
    span: &Range<usize>,
    path: &mut Vec<Part>,
) -> Option<&'a Node> {
    // Table spans may cover only their headers, not their children.
    match node.get_ref() {
        Node::Table(entries) => {
            for (key, child) in entries {
                path.push(Part::Key(key.clone()));
                if let Some(found) = locate(child, span, path) {
                    return Some(found);
                }
                path.pop();
            }
        }
        Node::Array(entries) => {
            for (index, child) in entries.iter().enumerate() {
                path.push(Part::Index(index));
                if let Some(found) = locate(child, span, path) {
                    return Some(found);
                }
                path.pop();
            }
        }
        Node::Value => {}
    }
    let node_span = node.span();
    (node_span.start <= span.start && node_span.end >= span.end).then_some(node.get_ref())
}

fn remove(value: &mut toml::Value, path: &[Part]) -> bool {
    match path {
        [Part::Key(key)] => value
            .as_table_mut()
            .is_some_and(|table| table.remove(key).is_some()),
        [Part::Key(key), rest @ ..] => value
            .as_table_mut()
            .and_then(|table| table.get_mut(key))
            .is_some_and(|child| remove(child, rest)),
        [Part::Index(index), rest @ ..] => value
            .as_array_mut()
            .and_then(|array| array.get_mut(*index))
            .is_some_and(|child| remove(child, rest)),
        [] => false,
    }
}

fn table_at_mut<'a>(value: &'a mut toml::Value, path: &[Part]) -> Option<&'a mut toml::Table> {
    match path {
        [] => value.as_table_mut(),
        [Part::Key(key), rest @ ..] => table_at_mut(value.as_table_mut()?.get_mut(key)?, rest),
        [Part::Index(index), rest @ ..] => {
            table_at_mut(value.as_array_mut()?.get_mut(*index)?, rest)
        }
    }
}

fn isolated_error(value: &toml::Value, path: &[Part]) -> Option<String> {
    let text = toml::to_string(value).ok()?;
    let error = Config::parse(&text).err()?;
    let tree: toml::Spanned<Node> = toml::from_str(&text).ok()?;
    let mut error_path = Vec::new();
    locate(&tree, &error.span()?, &mut error_path)?;
    error_path
        .starts_with(path)
        .then(|| error.message().to_owned())
}

fn node_at<'a>(node: &'a toml::Spanned<Node>, path: &[Part]) -> Option<&'a toml::Spanned<Node>> {
    match path {
        [] => Some(node),
        [Part::Key(key), rest @ ..] => match node.get_ref() {
            Node::Table(entries) => node_at(entries.get(key)?, rest),
            _ => None,
        },
        [Part::Index(index), rest @ ..] => match node.get_ref() {
            Node::Array(entries) => node_at(entries.get(*index)?, rest),
            _ => None,
        },
    }
}

fn display_path(path: &[Part]) -> String {
    let mut result = String::new();
    for part in path {
        match part {
            Part::Key(key) => {
                if !result.is_empty() {
                    result.push('.');
                }
                result.push_str(key);
            }
            Part::Index(index) => result.push_str(&format!("[{index}]")),
        }
    }
    result
}

pub(super) fn recover(content: &str) -> Option<(Config, Vec<DroppedField>)> {
    let original: toml::Spanned<Node> = toml::from_str(content).ok()?;
    let table: toml::Table = toml::from_str(content).ok()?;
    let mut value = toml::Value::Table(table);
    let mut current = toml::to_string(&value).ok()?;
    let mut error = Config::parse(&current).err()?;
    let mut dropped = Vec::new();
    loop {
        let tree: toml::Spanned<Node> = toml::from_str(&current).ok()?;
        let mut path = Vec::new();
        let node = locate(&tree, &error.span()?, &mut path)?;
        let mut candidates = Vec::new();
        if let Node::Table(entries) = node {
            let mut empty = value.clone();
            table_at_mut(&mut empty, &path)?.clear();
            if matches!(path.last(), Some(Part::Key(_)))
                && isolated_error(&empty, &path).as_deref() == Some(error.message())
            {
                candidates.push((path.clone(), error.message().to_owned()));
            } else {
                for key in entries.keys() {
                    let mut trial = value.clone();
                    table_at_mut(&mut trial, &path)?.retain(|name, _| name == key);
                    if let Some(reason) = isolated_error(&trial, &path) {
                        let mut candidate = path.clone();
                        candidate.push(Part::Key(key.clone()));
                        candidates.push((candidate, reason));
                    }
                }
            }
        } else {
            while matches!(path.last(), Some(Part::Index(_))) {
                path.pop();
            }
            if matches!(path.last(), Some(Part::Key(_))) {
                candidates.push((path, error.message().to_owned()));
            }
        }
        if candidates.is_empty() || dropped.len() + candidates.len() > 64 {
            return None;
        }
        for (selected, reason) in candidates {
            let original_span = node_at(&original, &selected)?.span();
            let prefix = content.get(..original_span.start)?;
            let location = Some((
                prefix.bytes().filter(|&byte| byte == b'\n').count() + 1,
                prefix.rsplit('\n').next()?.chars().count() + 1,
            ));
            dropped.push(DroppedField {
                path: display_path(&selected),
                location,
                reason,
            });
            if !remove(&mut value, &selected) {
                return None;
            }
        }
        current = toml::to_string(&value).ok()?;
        match Config::parse(&current) {
            Ok(config) => return Some((config, dropped)),
            Err(next) => error = next,
        }
    }
}
