use std::collections::HashMap;

use quick_xml::events::Event;

use crate::core::xml;

/// All numbering definitions from `word/numbering.xml`.
#[derive(Debug, Clone, Default)]
pub struct NumberingDefinitions {
    /// Abstract numbering definitions, keyed by abstract ID.
    pub abstract_nums: HashMap<u32, AbstractNum>,
    /// Concrete numbering instances, keyed by `numId`.
    pub instances: HashMap<u32, NumberingInstance>,
}

/// An abstract numbering definition.
#[derive(Debug, Clone)]
pub struct AbstractNum {
    /// Abstract numbering ID.
    pub abstract_num_id: u32,
    /// Per-level definitions, keyed by level index (0-based).
    pub levels: HashMap<u8, NumberingLevel>,
}

/// A single level within a numbering definition.
#[derive(Debug, Clone)]
pub struct NumberingLevel {
    /// Starting value for this level.
    pub start: u32,
    /// Number format (decimal, bullet, roman, etc.).
    pub format: NumberFormat,
}

/// A concrete numbering instance referencing an abstract definition.
#[derive(Debug, Clone)]
pub struct NumberingInstance {
    /// Numbering instance ID referenced by paragraphs.
    pub num_id: u32,
    /// ID of the abstract numbering this instance references.
    pub abstract_num_id: u32,
    /// Level overrides within this instance.
    pub overrides: HashMap<u8, NumberingLevel>,
}

/// Number format type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NumberFormat {
    /// Arabic decimal numbers (1, 2, 3, …).
    Decimal,
    /// Bullet character.
    Bullet,
    /// Lowercase letters (a, b, c, …).
    LowerLetter,
    /// Uppercase letters (A, B, C, …).
    UpperLetter,
    /// Lowercase Roman numerals (i, ii, iii, …).
    LowerRoman,
    /// Uppercase Roman numerals (I, II, III, …).
    UpperRoman,
    /// No numbering.
    None,
    /// Any other format value.
    Other(String),
}

impl NumberingDefinitions {
    /// Parse `word/numbering.xml` content.
    pub fn parse(xml_data: &[u8]) -> crate::core::Result<Self> {
        let mut reader = xml::make_fast_reader(xml_data);
        let mut defs = NumberingDefinitions::default();
        let mut item_count = 0_usize;

        loop {
            crate::budget::check_cancelled().map_err(crate::map_office_error_to_core)?;
            match reader.read_event()? {
                Event::Start(ref e) => match e.local_name().as_ref() {
                    b"abstractNum" => {
                        if let Some(an) = parse_abstract_num(&mut reader, e)? {
                            item_count =
                                item_count.saturating_add(an.levels.len()).saturating_add(1);
                            crate::budget::check_model_items(
                                "office_docx_numbering_items",
                                item_count,
                            )
                            .map_err(crate::map_office_error_to_core)?;
                            if defs.abstract_nums.insert(an.abstract_num_id, an).is_some() {
                                return Err(crate::core::Error::MalformedXml(
                                    "duplicate abstract numbering id".to_owned(),
                                ));
                            }
                        }
                    }
                    b"num" => {
                        if let Some(inst) = parse_num_instance(&mut reader, e)? {
                            item_count = item_count.saturating_add(1);
                            crate::budget::check_model_items(
                                "office_docx_numbering_items",
                                item_count,
                            )
                            .map_err(crate::map_office_error_to_core)?;
                            if defs.instances.insert(inst.num_id, inst).is_some() {
                                return Err(crate::core::Error::MalformedXml(
                                    "duplicate numbering instance id".to_owned(),
                                ));
                            }
                        }
                    }
                    _ => {}
                },
                Event::Eof => break,
                _ => {}
            }
        }
        Ok(defs)
    }

    /// Resolve a numbering level for a given numId + ilvl.
    pub fn resolve_level(&self, num_id: u32, ilvl: u8) -> Option<&NumberingLevel> {
        let instance = self.instances.get(&num_id)?;
        if let Some(level) = instance.overrides.get(&ilvl) {
            return Some(level);
        }
        let abstract_num = self.abstract_nums.get(&instance.abstract_num_id)?;
        abstract_num.levels.get(&ilvl)
    }
}

fn parse_abstract_num(
    reader: &mut quick_xml::Reader<&[u8]>,
    start: &quick_xml::events::BytesStart,
) -> crate::core::Result<Option<AbstractNum>> {
    let abstract_num_id = match xml::optional_attr_str(start, b"w:abstractNumId")? {
        Some(id) => id.parse()?,
        None => return Ok(None),
    };
    let mut levels = HashMap::new();

    loop {
        match reader.read_event()? {
            Event::Start(ref e) => {
                if e.local_name().as_ref() == b"lvl" {
                    let ilvl = xml::required_attr_str(e, b"w:ilvl")?.parse::<u8>()?;
                    let level = parse_numbering_level(reader)?;
                    if levels.insert(ilvl, level).is_some() {
                        return Err(crate::core::Error::MalformedXml(
                            "duplicate numbering level".to_owned(),
                        ));
                    }
                } else {
                    xml::skip_element_fast(reader)?;
                }
            }
            Event::End(ref e) if e.local_name().as_ref() == b"abstractNum" => {
                break;
            }
            Event::Eof => {
                return Err(crate::core::Error::MalformedXml(
                    "truncated abstract numbering definition".to_owned(),
                ));
            }
            _ => {}
        }
    }

    Ok(Some(AbstractNum {
        abstract_num_id,
        levels,
    }))
}

fn parse_numbering_level(
    reader: &mut quick_xml::Reader<&[u8]>,
) -> crate::core::Result<NumberingLevel> {
    let mut start_val = 1u32;
    let mut format = NumberFormat::Decimal;

    loop {
        crate::budget::check_cancelled().map_err(crate::map_office_error_to_core)?;
        match reader.read_event()? {
            Event::Start(ref e) | Event::Empty(ref e) => {
                match e.local_name().as_ref() {
                    b"start" => {
                        if let Some(val) = xml::optional_attr_str(e, b"w:val")? {
                            start_val = val.parse()?;
                        }
                    }
                    b"numFmt" => {
                        if let Some(val) = xml::optional_attr_str(e, b"w:val")? {
                            format = parse_number_format(&val);
                        }
                    }
                    b"pPr" | b"rPr" => {
                        // Skip sub-properties for now (they apply to the numbering marker)
                    }
                    _ => {}
                }
            }
            Event::End(ref e) if e.local_name().as_ref() == b"lvl" => {
                break;
            }
            Event::Eof => {
                return Err(crate::core::Error::MalformedXml(
                    "truncated numbering level".to_owned(),
                ));
            }
            _ => {}
        }
    }

    Ok(NumberingLevel {
        start: start_val,
        format,
    })
}

fn parse_number_format(val: &str) -> NumberFormat {
    match val {
        "decimal" => NumberFormat::Decimal,
        "bullet" => NumberFormat::Bullet,
        "lowerLetter" => NumberFormat::LowerLetter,
        "upperLetter" => NumberFormat::UpperLetter,
        "lowerRoman" => NumberFormat::LowerRoman,
        "upperRoman" => NumberFormat::UpperRoman,
        "none" => NumberFormat::None,
        other => NumberFormat::Other(other.to_string()),
    }
}

fn parse_num_instance(
    reader: &mut quick_xml::Reader<&[u8]>,
    start: &quick_xml::events::BytesStart,
) -> crate::core::Result<Option<NumberingInstance>> {
    let num_id = match xml::optional_attr_str(start, b"w:numId")? {
        Some(id) => id.parse()?,
        None => return Ok(None),
    };
    let mut abstract_num_id = None;
    let overrides = HashMap::new();

    loop {
        match reader.read_event()? {
            Event::Start(ref e) | Event::Empty(ref e)
                if e.local_name().as_ref() == b"abstractNumId" =>
            {
                abstract_num_id = Some(xml::required_attr_str(e, b"w:val")?.parse()?);
            }
            Event::End(ref e) if e.local_name().as_ref() == b"num" => {
                break;
            }
            Event::Eof => {
                return Err(crate::core::Error::MalformedXml(
                    "truncated numbering instance".to_owned(),
                ));
            }
            _ => {}
        }
    }

    Ok(Some(NumberingInstance {
        num_id,
        abstract_num_id: abstract_num_id.ok_or_else(|| crate::core::Error::MissingAttribute {
            element: "num/abstractNumId".to_owned(),
            attr: "w:val".to_owned(),
        })?,
        overrides,
    }))
}

#[cfg(any())]
mod tests {
    use super::*;

    const SAMPLE_NUMBERING: &[u8] = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:abstractNum w:abstractNumId="0">
    <w:lvl w:ilvl="0">
      <w:start w:val="1"/>
      <w:numFmt w:val="bullet"/>
      <w:lvlText w:val="&#61623;"/>
    </w:lvl>
    <w:lvl w:ilvl="1">
      <w:start w:val="1"/>
      <w:numFmt w:val="decimal"/>
      <w:lvlText w:val="%2."/>
    </w:lvl>
  </w:abstractNum>
  <w:num w:numId="1">
    <w:abstractNumId w:val="0"/>
  </w:num>
</w:numbering>"#;

    #[test]
    fn parse_numbering_defs() {
        let defs = NumberingDefinitions::parse(SAMPLE_NUMBERING).unwrap();
        assert_eq!(defs.abstract_nums.len(), 1);
        assert_eq!(defs.instances.len(), 1);

        let an = defs.abstract_nums.get(&0).unwrap();
        assert_eq!(an.levels.len(), 2);

        let inst = defs.instances.get(&1).unwrap();
        assert_eq!(inst.abstract_num_id, 0);
    }

    #[test]
    fn resolve_numbering_level() {
        let defs = NumberingDefinitions::parse(SAMPLE_NUMBERING).unwrap();
        let level = defs.resolve_level(1, 0).unwrap();
        assert_eq!(level.format, NumberFormat::Bullet);
        assert_eq!(level.start, 1);

        let level1 = defs.resolve_level(1, 1).unwrap();
        assert_eq!(level1.format, NumberFormat::Decimal);
    }
}
