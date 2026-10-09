use quick_xml::Reader;
use quick_xml::events::{BytesRef, Event};

#[derive(Debug, Clone, Default)]
pub struct Attribute {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, Default)]
pub struct Element {
    pub name: String,
    pub attributes: Vec<Attribute>,
    pub text: String,
    pub children: Vec<Element>,
    pub inner: Span,
}

impl Element {
    pub fn child(&self, name: &str) -> Option<&Element> {
        self.children.iter().find(|child| child.name == name)
    }

    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Element> {
        self.children.iter().filter(move |child| child.name == name)
    }

    pub fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|attribute| attribute.name == name)
            .map(|attribute| attribute.value.as_str())
    }

    pub fn path(&self, names: &[&str]) -> Option<&Element> {
        names.iter().try_fold(self, |element, name| element.child(name))
    }
}

pub fn parse(text: &str) -> Result<Element, String> {
    let mut reader = Reader::from_str(text);
    let mut stack: Vec<Element> = vec![Element::default()];
    loop {
        let before = usize::try_from(reader.buffer_position()).unwrap_or(0);
        match reader.read_event().map_err(|e| e.to_string())? {
            Event::Start(start) => {
                let mut element = element_of(&start)?;
                element.inner.start = usize::try_from(reader.buffer_position()).unwrap_or(0);
                stack.push(element);
            }
            Event::Empty(start) => {
                let element = element_of(&start)?;
                if let Some(parent) = stack.last_mut() {
                    parent.children.push(element);
                }
            }
            Event::End(_) => {
                let mut element = stack.pop().ok_or("unexpected end element")?;
                element.inner.end = before;
                stack.last_mut().ok_or("unexpected end element")?.children.push(element);
            }
            Event::Text(content) => append(&mut stack, &content.decode().map_err(|e| e.to_string())?),
            Event::CData(content) => append(&mut stack, &content.decode().map_err(|e| e.to_string())?),
            Event::GeneralRef(reference) => {
                let resolved = reference_text(&reference)?;
                append(&mut stack, &resolved);
            }
            Event::Eof => break,
            _ => {}
        }
    }
    let mut root = stack.pop().ok_or("empty document")?;
    match root.children.len() {
        0 => Err("empty document".to_owned()),
        _ => Ok(root.children.remove(0)),
    }
}

fn element_of(start: &quick_xml::events::BytesStart) -> Result<Element, String> {
    let attributes = start
        .attributes()
        .map(|attribute| {
            let attribute = attribute.map_err(|e| e.to_string())?;
            let value = attribute
                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .map_err(|e| e.to_string())?;
            let name = String::from_utf8_lossy(attribute.key.local_name().as_ref()).into_owned();
            Ok(Attribute {
                name,
                value: value.into_owned(),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(Element {
        name: String::from_utf8_lossy(start.local_name().as_ref()).into_owned(),
        attributes,
        ..Element::default()
    })
}

fn append(stack: &mut [Element], text: &str) {
    if let Some(top) = stack.last_mut() {
        top.text.push_str(text);
    }
}

fn reference_text(reference: &BytesRef) -> Result<String, String> {
    match reference.resolve_char_ref().map_err(|e| e.to_string())? {
        Some(character) => Ok(character.to_string()),
        None => reference
            .decode()
            .map(|name| entity(&name).to_owned())
            .map_err(|e| e.to_string()),
    }
}

fn entity(name: &str) -> &str {
    match name {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elements_expose_local_names_attributes_text_and_inner_ranges() {
        let xml = r#"<a:Root xmlns:a="urn:x" Id="1"><a:Item k="v">one &amp; two</a:Item><Empty/></a:Root>"#;

        let root = parse(xml).unwrap();

        assert_eq!(root.name, "Root");
        assert_eq!(root.attribute("Id"), Some("1"));
        assert_eq!(root.attribute("a"), Some("urn:x"));
        let item = root.child("Item").unwrap();
        assert_eq!(item.text, "one & two");
        assert_eq!(item.attribute("k"), Some("v"));
        assert_eq!(&xml[item.inner.start..item.inner.end], "one &amp; two");
        assert!(root.child("Empty").is_some());
    }

    #[test]
    fn numeric_character_references_are_decoded() {
        let root = parse("<a>&#65;&#x42;&amp;</a>").unwrap();

        assert_eq!(root.text, "AB&");
    }
}
