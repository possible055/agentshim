use std::collections::HashMap;

use quick_xml::events::Event;

use super::error::{Error, Result};
use super::opc::PartName;
use super::xml;

/// Standard relationship type URIs (Transitional / ECMA-376).
pub mod rel_types {
    /// Relationship type for the main office document part.
    pub const OFFICE_DOCUMENT: &str =
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument";
    /// Relationship type for `styles.xml`.
    pub const STYLES: &str =
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles";
    /// Relationship type for a SpreadsheetML / DrawingML drawing part
    /// (`xl/drawings/drawingN.xml`). Worksheet-to-drawing rel; the
    /// drawing itself owns IMAGE rels keyed by `<a:blip r:embed=...>`.
    pub const DRAWING: &str =
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing";
    /// Relationship type for `numbering.xml`.
    pub const NUMBERING: &str =
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/numbering";
    /// Relationship type for notes slide parts.
    pub const NOTES_SLIDE: &str =
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/notesSlide";
}

/// Strict (ISO 29500) relationship type prefix.
const STRICT_REL_PREFIX: &str = "http://purl.oclc.org/ooxml/officeDocument/relationships/";
/// Transitional relationship type prefix.
const TRANSITIONAL_REL_PREFIX: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/";

/// Normalize a Strict relationship type URI to its Transitional equivalent.
/// Strict uses `http://purl.oclc.org/ooxml/officeDocument/relationships/...`
/// Transitional uses `http://schemas.openxmlformats.org/officeDocument/2006/relationships/...`
/// If it's already Transitional or an unrecognized URI, return as-is.
fn normalize_rel_type(rel_type: String) -> String {
    if let Some(suffix) = rel_type.strip_prefix(STRICT_REL_PREFIX) {
        format!("{TRANSITIONAL_REL_PREFIX}{suffix}")
    } else {
        rel_type
    }
}

/// Indicates whether a relationship target is inside or outside the package.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetMode {
    /// Target is another part within the same OPC package.
    Internal,
    /// Target is an external resource (e.g., a hyperlink URL).
    External,
}

/// A single relationship entry from a `.rels` file.
#[derive(Debug, Clone)]
pub struct Relationship {
    /// Relationship ID (`rId1`, `rId2`, …).
    pub id: String,
    /// Relationship type URI.
    pub rel_type: String,
    /// Target path or URL.
    pub target: String,
    /// Whether the target is internal or external.
    pub target_mode: TargetMode,
}

/// Parsed collection of relationships from a `.rels` file.
#[derive(Debug, Clone)]
pub struct Relationships {
    rels: Vec<Relationship>,
    by_id: HashMap<String, usize>,
    by_type: HashMap<String, Vec<usize>>,
}

impl Relationships {
    /// Parse a `.rels` XML file.
    pub fn parse(xml_data: &[u8]) -> Result<Self> {
        let mut reader = xml::make_fast_reader(xml_data);
        let mut rels = Vec::new();

        loop {
            crate::budget::check_cancelled().map_err(crate::map_office_error_to_core)?;
            match reader.read_event()? {
                Event::Start(ref e) | Event::Empty(ref e)
                    if e.local_name().as_ref() == b"Relationship" =>
                {
                    let id = xml::required_attr_str(e, b"Id")?.into_owned();
                    let rel_type =
                        normalize_rel_type(xml::required_attr_str(e, b"Type")?.into_owned());
                    let target = xml::required_attr_str(e, b"Target")?.into_owned();
                    let target_mode = match xml::optional_attr_str(e, b"TargetMode")? {
                        Some(ref mode) if mode.eq_ignore_ascii_case("External") => {
                            TargetMode::External
                        }
                        Some(ref mode) if mode.eq_ignore_ascii_case("Internal") => {
                            TargetMode::Internal
                        }
                        Some(_) => {
                            return Err(Error::MalformedXml(
                                "invalid relationship TargetMode".to_owned(),
                            ));
                        }
                        None => TargetMode::Internal,
                    };
                    crate::budget::charge_opc_item("office_relationships")
                        .map_err(crate::map_office_error_to_core)?;
                    crate::budget::charge_opc_text(
                        "office_relationship_text_bytes",
                        id.len()
                            .saturating_add(rel_type.len())
                            .saturating_add(target.len()),
                    )
                    .map_err(crate::map_office_error_to_core)?;
                    rels.push(Relationship {
                        id,
                        rel_type,
                        target,
                        target_mode,
                    });
                    crate::budget::check_model_items("office_relationships", rels.len())
                        .map_err(crate::map_office_error_to_core)?;
                }
                Event::Eof => break,
                _ => {}
            }
        }

        Self::from_vec(rels)
    }

    /// Create an empty relationships collection.
    pub fn empty() -> Self {
        Self {
            rels: Vec::new(),
            by_id: HashMap::new(),
            by_type: HashMap::new(),
        }
    }

    fn from_vec(rels: Vec<Relationship>) -> Result<Self> {
        let mut by_id = HashMap::with_capacity(rels.len());
        let mut by_type: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, r) in rels.iter().enumerate() {
            crate::budget::charge_opc_text(
                "office_relationship_index_bytes",
                r.id.len().saturating_add(r.rel_type.len()),
            )
            .map_err(crate::map_office_error_to_core)?;
            if by_id.insert(r.id.clone(), i).is_some() {
                return Err(Error::MalformedXml("duplicate relationship id".to_owned()));
            }
            by_type.entry(r.rel_type.clone()).or_default().push(i);
        }
        Ok(Self {
            rels,
            by_id,
            by_type,
        })
    }

    /// Look up a relationship by its ID.
    pub fn get_by_id(&self, id: &str) -> Option<&Relationship> {
        self.by_id.get(id).map(|&i| &self.rels[i])
    }

    /// Return the first relationship with the given type URI.
    pub fn first_by_type(&self, rel_type: &str) -> Option<&Relationship> {
        self.by_type
            .get(rel_type)
            .and_then(|indices| indices.first().map(|&i| &self.rels[i]))
    }

    /// Resolve an internal relationship target relative to a source part name.
    ///
    /// For package-level relationships (source is root), pass a source of `PartName::new("/").ok()`
    /// or use `resolve_target_from_root`.
    pub fn resolve_target(&self, id: &str, source: &PartName) -> Result<PartName> {
        let rel = self
            .get_by_id(id)
            .ok_or_else(|| Error::RelationshipNotFound(id.to_string()))?;

        if rel.target_mode == TargetMode::External {
            return Err(Error::Unsupported(format!(
                "cannot resolve external target: {}",
                rel.target
            )));
        }

        source.resolve_relative(&rel.target)
    }
}

#[cfg(any())]
mod tests {
    use super::*;

    const SAMPLE_RELS: &[u8] = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1"
    Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument"
    Target="word/document.xml"/>
  <Relationship Id="rId2"
    Type="http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties"
    Target="docProps/core.xml"/>
  <Relationship Id="rId3"
    Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/extended-properties"
    Target="docProps/app.xml"/>
</Relationships>"#;

    #[test]
    fn parse_relationships() {
        let rels = Relationships::parse(SAMPLE_RELS).unwrap();
        assert_eq!(rels.all().len(), 3);
    }

    #[test]
    fn get_by_id() {
        let rels = Relationships::parse(SAMPLE_RELS).unwrap();
        let r = rels.get_by_id("rId1").unwrap();
        assert_eq!(r.target, "word/document.xml");
        assert_eq!(r.target_mode, TargetMode::Internal);
    }

    #[test]
    fn get_by_type() {
        let rels = Relationships::parse(SAMPLE_RELS).unwrap();
        let docs = rels.get_by_type(rel_types::OFFICE_DOCUMENT);
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].target, "word/document.xml");
    }

    #[test]
    fn first_by_type() {
        let rels = Relationships::parse(SAMPLE_RELS).unwrap();
        let r = rels.first_by_type(rel_types::CORE_PROPERTIES).unwrap();
        assert_eq!(r.target, "docProps/core.xml");
    }

    #[test]
    fn resolve_target_from_root() {
        let rels = Relationships::parse(SAMPLE_RELS).unwrap();
        let part = rels.resolve_target_from_root("rId1").unwrap();
        assert_eq!(part.as_str(), "/word/document.xml");
    }

    #[test]
    fn builder_round_trip() {
        let mut builder = RelationshipsBuilder::new();
        let id = builder.add(rel_types::OFFICE_DOCUMENT, "word/document.xml");
        assert_eq!(id, "rId1");

        let xml = builder.serialize();
        let rels = Relationships::parse(&xml).unwrap();
        assert_eq!(rels.all().len(), 1);
        assert_eq!(
            rels.get_by_id("rId1").unwrap().rel_type,
            rel_types::OFFICE_DOCUMENT
        );
    }
}
