// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Language configuration.

use serde::Deserialize;
use serde::de::{self, Deserializer};

/// A server reference within a language binding.
///
/// Supports both bare string form (`"foo"`) and inline-table form
/// (`{ name = "foo", diagnostics = false }`). Bare strings expand
/// to `{ name, diagnostics: true }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerBinding {
    /// Server name (references a `[server.*]` entry).
    pub name: String,

    /// Whether this server delivers diagnostics for this language.
    /// Defaults to `true`.
    pub diagnostics: bool,
}

impl ServerBinding {
    /// Creates a new binding with diagnostics enabled (the default).
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            diagnostics: true,
        }
    }
}

impl<'de> Deserialize<'de> for ServerBinding {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ServerBindingVisitor;

        impl<'de> de::Visitor<'de> for ServerBindingVisitor {
            type Value = ServerBinding;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(
                    "a server name string or inline table \
                     { name = \"...\", diagnostics = ... }",
                )
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(ServerBinding {
                    name: v.to_string(),
                    diagnostics: true,
                })
            }

            fn visit_map<A: de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut name: Option<String> = None;
                let mut diagnostics: Option<bool> = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "name" => {
                            if name.is_some() {
                                return Err(de::Error::duplicate_field("name"));
                            }
                            name = Some(map.next_value()?);
                        }
                        "diagnostics" => {
                            if diagnostics.is_some() {
                                return Err(de::Error::duplicate_field("diagnostics"));
                            }
                            diagnostics = Some(map.next_value()?);
                        }
                        other => {
                            return Err(de::Error::unknown_field(other, &["name", "diagnostics"]));
                        }
                    }
                }

                let name = name.ok_or_else(|| de::Error::missing_field("name"))?;
                Ok(ServerBinding {
                    name,
                    diagnostics: diagnostics.unwrap_or(true),
                })
            }
        }

        deserializer.deserialize_any(ServerBindingVisitor)
    }
}

/// Per-language configuration for how Catenary handles a language.
///
/// Each entry references one or more server definitions from `[server.*]`
/// via the `servers` list and controls diagnostic severity filtering.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct LanguageConfig {
    /// Ordered list of server bindings (references `[server.*]` entries).
    /// Order defines dispatch priority.
    pub servers: Vec<ServerBinding>,

    /// Whether to deliver diagnostics for this language.
    /// Defaults to `true`. AND with per-binding `diagnostics`
    /// to determine effective delivery per server.
    pub diagnostics: bool,

    /// Minimum diagnostic severity to deliver to agents.
    /// Valid values: `"error"`, `"warning"`, `"information"`, `"hint"`.
    /// When absent, all severities are delivered.
    pub min_severity: Option<String>,
}

impl Default for LanguageConfig {
    fn default() -> Self {
        Self {
            servers: Vec::new(),
            diagnostics: true,
            min_severity: None,
        }
    }
}

impl LanguageConfig {
    /// Whether diagnostics from `server_name` should be delivered
    /// for this language binding.
    ///
    /// Returns `false` if the server is not in the bindings list or
    /// if either the language-level or per-binding `diagnostics` flag
    /// is `false`.
    #[must_use]
    pub fn diagnostics_enabled(&self, server_name: &str) -> bool {
        self.diagnostics
            && self
                .servers
                .iter()
                .find(|b| b.name == server_name)
                .is_some_and(|b| b.diagnostics)
    }
}
