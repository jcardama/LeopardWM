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
                while let Some((key, value)) = map.next_entry()? {
                    result.insert(key, value);
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

fn descendants(node: &Node, path: &mut Vec<Part>, result: &mut Vec<Vec<Part>>) {
    match node {
        Node::Table(entries) => {
            for (key, child) in entries {
                path.push(Part::Key(key.clone()));
                descendants(child.get_ref(), path, result);
                path.pop();
            }
        }
        Node::Array(entries)
            if !entries.is_empty()
                && entries
                    .iter()
                    .all(|child| matches!(child.get_ref(), Node::Table(_))) =>
        {
            for (index, child) in entries.iter().enumerate() {
                path.push(Part::Index(index));
                descendants(child.get_ref(), path, result);
                path.pop();
            }
        }
        _ => {
            if matches!(path.last(), Some(Part::Key(_))) {
                result.push(path.clone());
            }
        }
    }
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
    // Compare trial errors against the same serialized table order.
    let mut current = toml::to_string(&value).ok()?;
    let mut error = Config::parse(&current).err()?;
    let mut dropped = Vec::new();
    for _ in 0..64 {
        let tree: toml::Spanned<Node> = toml::from_str(&current).ok()?;
        let mut path = Vec::new();
        let mut node = locate(&tree, &error.span()?, &mut path)?;
        if matches!(node, Node::Value) && matches!(path.last(), Some(Part::Index(_))) {
            while matches!(path.last(), Some(Part::Index(_))) {
                path.pop();
            }
            node = node_at(&tree, &path)?.get_ref();
        }
        let mut candidates = Vec::new();
        descendants(node, &mut path, &mut candidates);
        if candidates.len() != 1 || !matches!(node, Node::Value) {
            candidates.retain(|candidate| {
                let mut trial = value.clone();
                if !remove(&mut trial, candidate) {
                    return false;
                }
                let Ok(text) = toml::to_string(&trial) else {
                    return false;
                };
                match Config::parse(&text) {
                    Ok(_) => true,
                    Err(next) => next.message() != error.message(),
                }
            });
        }
        if candidates.len() != 1 {
            return None;
        }
        let selected = candidates.pop()?;
        let original_span = node_at(&original, &selected)?.span();
        let prefix = content.get(..original_span.start)?;
        let location = Some((
            prefix.bytes().filter(|&byte| byte == b'\n').count() + 1,
            prefix.rsplit('\n').next()?.chars().count() + 1,
        ));
        dropped.push(DroppedField {
            path: display_path(&selected),
            location,
            reason: error.message().to_string(),
        });
        if !remove(&mut value, &selected) {
            return None;
        }
        current = toml::to_string(&value).ok()?;
        match Config::parse(&current) {
            Ok(config) => return Some((config, dropped)),
            Err(next) => error = next,
        }
    }
    None
}
