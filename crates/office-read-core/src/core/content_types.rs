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
        if let Some(ext) = part_name.extension()
            && let Some(ct) = self.defaults.get(&ext.to_ascii_lowercase())
        {
            return Some(ct.as_str());
        }
        None
    }

    /// Return the override content-type map keyed by part name.
    pub fn overrides(&self) -> &HashMap<PartName, String> {
        &self.overrides
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_overrides_and_rejects_duplicate_content_types() {
        let types = ContentTypes::parse(
            br#"<Types>
            <Default Extension="xml" ContentType="application/xml"/>
            <Override PartName="/word/document.xml" ContentType="application/word"/>
            </Types>"#,
        )
        .expect("content types");
        assert_eq!(
            types.resolve(&PartName::new("/word/document.xml").expect("part")),
            Some("application/word")
        );
        assert_eq!(
            types.resolve(&PartName::new("/notes.xml").expect("part")),
            Some("application/xml")
        );
        let duplicate = br#"<Types>
            <Default Extension="xml" ContentType="application/xml"/>
            <Default Extension="xml" ContentType="application/word"/>
            </Types>"#;
        assert!(matches!(
            ContentTypes::parse(duplicate),
            Err(crate::core::error::Error::MalformedXml(_))
        ));
    }
}
