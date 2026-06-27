//! Static MCP resources served by the rest-e2e-mcp server.
//!
//! Resources are embedded at compile time via `include_str!` so that the
//! published crate is self-contained (see `Cargo.toml` `include` list).
//! The catalog below is the single source of truth for both
//! `resources/list` and `resources/read`.

use rmcp::model::{
    Annotated, ListResourcesResult, RawResource, ReadResourceResult, Resource, ResourceContents,
};

// ----------------------------- Embedded content -----------------------------

const GETTING_STARTED: &str = include_str!("../../resources/guides/getting-started.md");
const YAML_FORMAT: &str = include_str!("../../resources/guides/yaml-format.md");
const VARIABLES: &str = include_str!("../../resources/guides/variables.md");
const ASSERTIONS: &str = include_str!("../../resources/guides/assertions.md");

const SCHEMA_YAML: &str = include_str!("../../resources/spec/testsuite.schema.yaml");

const EXAMPLE_MINIMAL: &str = include_str!("../../resources/examples/minimal.yaml");
const EXAMPLE_GITHUB: &str = include_str!("../../resources/examples/github-api.yaml");

// --------------------------------- Catalog ---------------------------------

struct ResourceEntry {
    uri: &'static str,
    name: &'static str,
    title: &'static str,
    description: &'static str,
    mime_type: &'static str,
    body: &'static str,
}

const CATALOG: &[ResourceEntry] = &[
    ResourceEntry {
        uri: "rest-e2e://guide/getting-started",
        name: "guide-getting-started",
        title: "Getting Started",
        description: "Five-minute introduction: write, run, and inspect a suite.",
        mime_type: "text/markdown",
        body: GETTING_STARTED,
    },
    ResourceEntry {
        uri: "rest-e2e://guide/yaml-format",
        name: "guide-yaml-format",
        title: "TestSuite YAML Format",
        description: "Field-by-field description of the TestSuite YAML structure.",
        mime_type: "text/markdown",
        body: YAML_FORMAT,
    },
    ResourceEntry {
        uri: "rest-e2e://guide/variables",
        name: "guide-variables",
        title: "Variables",
        description: "Variable syntax, the four sources, and tracing precedence.",
        mime_type: "text/markdown",
        body: VARIABLES,
    },
    ResourceEntry {
        uri: "rest-e2e://guide/assertions",
        name: "guide-assertions",
        title: "Assertions",
        description: "Semantics of expect.status / headers / body assertions.",
        mime_type: "text/markdown",
        body: ASSERTIONS,
    },
    ResourceEntry {
        uri: "rest-e2e://spec/testsuite.schema.yaml",
        name: "spec-testsuite-schema-yaml",
        title: "TestSuite JSON Schema (YAML form)",
        description: "Authoritative TestSuite schema in YAML. JSON form is emitted by the `schema` tool.",
        mime_type: "application/yaml",
        body: SCHEMA_YAML,
    },
    ResourceEntry {
        uri: "rest-e2e://example/minimal.yaml",
        name: "example-minimal",
        title: "Minimal Example Suite",
        description: "Smallest passing TestSuite (httpbin.org GET).",
        mime_type: "application/yaml",
        body: EXAMPLE_MINIMAL,
    },
    ResourceEntry {
        uri: "rest-e2e://example/github-api.yaml",
        name: "example-github-api",
        title: "GitHub API Example Suite",
        description: "Realistic multi-request suite probing the public GitHub REST API.",
        mime_type: "application/yaml",
        body: EXAMPLE_GITHUB,
    },
];

// --------------------------------- Helpers ---------------------------------

fn entry_to_resource(entry: &ResourceEntry) -> Resource {
    let raw = RawResource {
        uri: entry.uri.to_string(),
        name: entry.name.to_string(),
        title: Some(entry.title.to_string()),
        description: Some(entry.description.to_string()),
        mime_type: Some(entry.mime_type.to_string()),
        size: Some(entry.body.len() as u32),
        icons: None,
        meta: None,
    };
    Annotated::new(raw, None)
}

/// Build the `resources/list` response from the static catalog.
pub fn list_all() -> ListResourcesResult {
    let resources = CATALOG.iter().map(entry_to_resource).collect();
    ListResourcesResult {
        resources,
        next_cursor: None,
        meta: None,
    }
}

/// Build the `resources/read` response for a given URI, or `None` if not found.
pub fn read(uri: &str) -> Option<ReadResourceResult> {
    let entry = CATALOG.iter().find(|e| e.uri == uri)?;
    let contents = ResourceContents::TextResourceContents {
        uri: entry.uri.to_string(),
        mime_type: Some(entry.mime_type.to_string()),
        text: entry.body.to_string(),
        meta: None,
    };
    Some(ReadResourceResult {
        contents: vec![contents],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_expected_uris() {
        let uris: Vec<&str> = CATALOG.iter().map(|e| e.uri).collect();
        assert!(uris.contains(&"rest-e2e://guide/getting-started"));
        assert!(uris.contains(&"rest-e2e://spec/testsuite.schema.yaml"));
        assert!(uris.contains(&"rest-e2e://example/minimal.yaml"));
    }

    #[test]
    fn list_includes_all_entries() {
        let result = list_all();
        assert_eq!(result.resources.len(), CATALOG.len());
    }

    #[test]
    fn read_known_uri_returns_some() {
        let result = read("rest-e2e://guide/yaml-format").expect("known uri");
        assert_eq!(result.contents.len(), 1);
    }

    #[test]
    fn read_unknown_uri_returns_none() {
        assert!(read("rest-e2e://does-not-exist").is_none());
    }

    #[test]
    fn embedded_bodies_are_non_empty() {
        for entry in CATALOG {
            assert!(
                !entry.body.is_empty(),
                "resource {} has empty body",
                entry.uri
            );
        }
    }
}
