use std::collections::HashMap;

use axum::http::StatusCode;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Node {
    Text(String),
    Field(String),
    StatusText(String),
    Template(String),
    Block { name: String, fallback: Vec<Node> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Text(String),
    Action(String),
}

pub struct Template {
    root: Vec<Node>,
    definitions: HashMap<String, Vec<Node>>,
}

impl Template {
    pub fn parse(layout: &str, page: &str) -> Self {
        let mut definitions = HashMap::new();
        let root = parse_nodes(&mut tokenize(layout).into_iter(), &mut definitions);
        parse_nodes(&mut tokenize(page).into_iter(), &mut definitions);
        Self { root, definitions }
    }

    pub fn render(&self, field: &dyn Fn(&str) -> String) -> String {
        let mut output = String::new();
        self.render_nodes(&self.root, field, &mut output);
        output
    }

    fn render_nodes(&self, nodes: &[Node], field: &dyn Fn(&str) -> String, output: &mut String) {
        nodes.iter().for_each(|node| match node {
            Node::Text(text) => output.push_str(text),
            Node::Field(name) => output.push_str(&escape_html(&field(name))),
            Node::StatusText(name) => output.push_str(&status_text(&field(name))),
            Node::Template(name) => self.render_named(name, field, output),
            Node::Block { name, fallback } => match self.definitions.get(name) {
                Some(nodes) => self.render_nodes(nodes, field, output),
                None => self.render_nodes(fallback, field, output),
            },
        });
    }

    fn render_named(&self, name: &str, field: &dyn Fn(&str) -> String, output: &mut String) {
        if let Some(nodes) = self.definitions.get(name) {
            self.render_nodes(nodes, field, output);
        }
    }
}

fn status_text(code: &str) -> String {
    code.parse::<u16>()
        .ok()
        .and_then(|code| StatusCode::from_u16(code).ok())
        .and_then(|status| status.canonical_reason())
        .unwrap_or_default()
        .to_owned()
}

pub fn escape_html(raw: &str) -> String {
    raw.chars().fold(String::with_capacity(raw.len()), |mut escaped, c| {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&#34;"),
            '\'' => escaped.push_str("&#39;"),
            '+' => escaped.push_str("&#43;"),
            '\0' => escaped.push('\u{fffd}'),
            other => escaped.push(other),
        }
        escaped
    })
}

fn tokenize(source: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut rest = source;
    while let Some((before, after_open)) = rest.split_once("{{") {
        let (body, after) = after_open.split_once("}}").unwrap_or((after_open, ""));
        let trims_left = body.starts_with("- ");
        let text = if trims_left { before.trim_end() } else { before };
        if !text.is_empty() {
            tokens.push(Token::Text(text.to_owned()));
        }
        tokens.push(Token::Action(body.trim_start_matches('-').trim().to_owned()));
        rest = after;
    }
    if !rest.is_empty() {
        tokens.push(Token::Text(rest.to_owned()));
    }
    tokens
}

fn parse_nodes(tokens: &mut std::vec::IntoIter<Token>, definitions: &mut HashMap<String, Vec<Node>>) -> Vec<Node> {
    let mut nodes = Vec::new();
    while let Some(token) = tokens.next() {
        match token {
            Token::Text(text) => nodes.push(Node::Text(text)),
            Token::Action(action) if action == "end" => break,
            Token::Action(action) => parse_action(&action, tokens, definitions, &mut nodes),
        }
    }
    nodes
}

fn parse_action(
    action: &str,
    tokens: &mut std::vec::IntoIter<Token>,
    definitions: &mut HashMap<String, Vec<Node>>,
    nodes: &mut Vec<Node>,
) {
    let mut words = action.split_whitespace();
    match (words.next(), words.next()) {
        (Some("define"), Some(name)) => {
            let body = parse_nodes(tokens, definitions);
            definitions.insert(unquote(name), body);
        }
        (Some("block"), Some(name)) => {
            let fallback = parse_nodes(tokens, definitions);
            nodes.push(Node::Block {
                name: unquote(name),
                fallback,
            });
        }
        (Some("template"), Some(name)) => nodes.push(Node::Template(unquote(name))),
        (Some("statusText"), Some(field)) => nodes.push(Node::StatusText(field.trim_start_matches('.').to_owned())),
        (Some(field), None) => nodes.push(Node::Field(field.trim_start_matches('.').to_owned())),
        _ => {}
    }
}

fn unquote(quoted: &str) -> String {
    quoted.trim_matches('"').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(layout: &str, page: &str) -> String {
        Template::parse(layout, page).render(&|name| match name {
            "Name" => "<b>&'\"+".to_owned(),
            "Code" => "502".to_owned(),
            other => other.to_owned(),
        })
    }

    #[test]
    fn fills_fields_and_named_templates() {
        let layout = "<h1>{{template \"title\" .}}</h1>{{.Name}}";
        let page = "{{define \"title\"}}Hi {{.Code}}{{end}}";
        assert_eq!(render(layout, page), "<h1>Hi 502</h1>&lt;b&gt;&amp;&#39;&#34;&#43;");
    }

    #[test]
    fn block_falls_back_when_the_page_does_not_define_it() {
        let layout = "a{{- block \"actions\" .}}default{{end}}";
        assert_eq!(render(layout, ""), "adefault");
        assert_eq!(render(layout, "{{define \"actions\"}}custom{{end}}"), "acustom");
    }

    #[test]
    fn left_trim_removes_preceding_whitespace() {
        assert_eq!(render("x\n  {{- define \"a\"}}y{{end}}z", ""), "xz");
    }

    #[test]
    fn status_text_names_the_code() {
        assert_eq!(render("{{.Code}} {{statusText .Code}}", ""), "502 Bad Gateway");
    }
}
