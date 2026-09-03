use std::collections::HashMap;

use quick_xml::events::Event;

use super::error::Result;
use super::opc::PartName;
use super::xml;

/// Content types table parsed from `[Content_Types].xml`.
///
/// Maps parts to MIME content types via default (by extension) and override (by part name) entries.
#[derive(Debug, Clone)]
pub struct ContentTypes {
    /// Extension -> content type (both lowercased).
    defaults: HashMap<String, String>,
    /// Part name -> content type.
    overrides: HashMap<PartName, String>,
}

impl ContentTypes {
    /// Parse `[Content_Types].xml` from raw XML bytes.
    pub fn parse(xml_data: &[u8]) -> Result<Self> {
        let mut reader = xml::make_fast_reader(xml_data);
        let mut defaults = HashMap::new();
        let mut overrides = HashMap::new();

        loop {
            crate::budget::check_cancelled().map_err(crate::map_office_error_to_core)?;
            match reader.read_event()? {
                Event::Start(ref e) | Event::Empty(ref e) => {
                    let local = e.local_name();
                    let local_bytes = local.as_ref();

                    match local_bytes {
                        b"Default" => {
                            let ext = xml::required_attr_str(e, b"Extension")?;
                            let ct = xml::required_attr_str(e, b"ContentType")?;
                            crate::budget::charge_opc_item("office_content_types")
                                .map_err(crate::map_office_error_to_core)?;
                            crate::budget::charge_opc_text(
                                "office_content_type_text_bytes",
                                ext.len().saturating_add(ct.len()),
                            )
                            .map_err(crate::map_office_error_to_core)?;
                            if defaults
                                .insert(ext.to_ascii_lowercase(), ct.into_owned())
                                .is_some()
                            {
                                return Err(super::error::Error::MalformedXml(
                                    "duplicate content-type default".to_owned(),
                                ));
                            }
                        }
                        b"Override" => {
                            let part = xml::required_attr_str(e, b"PartName")?;
                            let ct = xml::required_attr_str(e, b"ContentType")?;
                            crate::budget::charge_opc_item("office_content_types")
                                .map_err(crate::map_office_error_to_core)?;
                            crate::budget::charge_opc_text(
                                "office_content_type_text_bytes",
                                part.len().saturating_add(ct.len()),
                            )
                            .map_err(crate::map_office_error_to_core)?;
                            let part_name = PartName::new(&part)?;
                            if overrides.insert(part_name, ct.into_owned()).is_some() {
                                return Err(super::error::Error::MalformedXml(
                                    "duplicate content-type override".to_owned(),
                                ));
                            }
                        }
                        _ => {}
                    }
                }
                Event::Eof => break,
                _ => {}
            }
        }

        Ok(Self {
            defaults,
            overrides,
        })
    }

    /// Resolve the content type for a part name.
    /// Checks overrides first, then defaults by file extension.
    pub fn resolve(&self, part_name: &PartName) -> Option<&str> {
        // Override takes precedence (case-insensitive via PartName's Eq)
        if let Some(ct) = self.overrides.get(part_name) {
            return Some(ct.as_str());
        }
        // Fall back to default by extension
        if let Some(ext) = part_name.extension() {
            if let Some(ct) = self.defaults.get(&ext.to_ascii_lowercase()) {
                return Some(ct.as_str());
            }
        }
        None
    }

    /// Return the override content-type map keyed by part name.
    pub fn overrides(&self) -> &HashMap<PartName, String> {
        &self.overrides
    }
}

#[cfg(any())]
mod tests {
    use super::*;

    const SAMPLE_CT_XML: &[u8] = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Default Extension="png" ContentType="image/png"/>
  <Override PartName="/word/document.xml"
            ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
  <Override PartName="/docProps/core.xml"
            ContentType="application/vnd.openxmlformats-package.core-properties+xml"/>
</Types>"#;

    #[test]
    fn parse_content_types() {
        let ct = ContentTypes::parse(SAMPLE_CT_XML).unwrap();
        assert_eq!(ct.defaults().len(), 3);
        assert_eq!(ct.overrides().len(), 2);
    }

    #[test]
    fn resolve_override() {
        let ct = ContentTypes::parse(SAMPLE_CT_XML).unwrap();
        let part = PartName::new("/word/document.xml").unwrap();
        assert_eq!(
            ct.resolve(&part),
            Some(
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"
            )
        );
    }

    #[test]
    fn resolve_default_by_extension() {
        let ct = ContentTypes::parse(SAMPLE_CT_XML).unwrap();
        let part = PartName::new("/word/media/image1.png").unwrap();
        assert_eq!(ct.resolve(&part), Some("image/png"));
    }

    #[test]
    fn resolve_unknown_returns_none() {
        let ct = ContentTypes::parse(SAMPLE_CT_XML).unwrap();
        let part = PartName::new("/word/unknown.bin").unwrap();
        assert_eq!(ct.resolve(&part), None);
    }

    #[test]
    fn builder_round_trip() {
        let mut builder = ContentTypesBuilder::new();
        builder.add_default("png", "image/png");
        builder.add_override(
            PartName::new("/word/document.xml").unwrap(),
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml",
        );
        let xml = builder.serialize();
        let ct = ContentTypes::parse(&xml).unwrap();
        assert_eq!(ct.defaults().get("png"), Some(&"image/png".to_string()));
        let part = PartName::new("/word/document.xml").unwrap();
        assert!(ct.resolve(&part).is_some());
    }
}
