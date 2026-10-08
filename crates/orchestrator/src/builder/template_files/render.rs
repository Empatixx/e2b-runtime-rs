use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Text(String),
    Number(i64),
    Flag(bool),
}

impl Value {
    fn truthy(&self) -> bool {
        match self {
            Value::Text(text) => !text.is_empty(),
            Value::Number(number) => *number != 0,
            Value::Flag(flag) => *flag,
        }
    }

    fn render(&self) -> String {
        match self {
            Value::Text(text) => text.clone(),
            Value::Number(number) => number.to_string(),
            Value::Flag(flag) => flag.to_string(),
        }
    }
}

pub type Values = BTreeMap<&'static str, Value>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrittenFile {
    pub path: String,
    pub mode: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rendered {
    pub text: String,
    pub files: Vec<WrittenFile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Condition {
    Field(String),
    Equals { field: String, literal: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Node {
    Text(String),
    Field(String),
    WriteFile(WrittenFile),
    If {
        condition: Condition,
        then: Vec<Node>,
        otherwise: Vec<Node>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Text(String),
    Action(String),
}

pub fn render(name: &str, source: &str, values: &Values) -> Result<Rendered, String> {
    let nodes = parse(&mut tokens(source).into_iter().peekable(), name)?;
    let mut rendered = Rendered::default();
    execute(&nodes, values, &mut rendered).map_err(|e| format!("error executing template {name:?}: {e}"))?;
    Ok(rendered)
}

fn tokens(source: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut rest = source;
    while let Some(start) = rest.find("{{") {
        let end = rest[start..].find("}}").map_or(rest.len(), |offset| start + offset);
        let (body, trim_left, trim_right) = action_body(&rest[start + 2..end]);
        let text = match trim_left {
            true => rest[..start].trim_end(),
            false => &rest[..start],
        };
        tokens.push(Token::Text(text.to_owned()));
        tokens.push(Token::Action(body.to_owned()));
        rest = &rest[(end + 2).min(rest.len())..];
        if trim_right {
            rest = rest.trim_start();
        }
    }
    tokens.push(Token::Text(rest.to_owned()));
    tokens
}

fn action_body(raw: &str) -> (&str, bool, bool) {
    let trim_left = raw.starts_with("- ");
    let trim_right = raw.ends_with(" -");
    let body = raw.strip_prefix("- ").unwrap_or(raw);
    let body = body.strip_suffix(" -").unwrap_or(body);
    (body.trim(), trim_left, trim_right)
}

fn parse(tokens: &mut std::iter::Peekable<std::vec::IntoIter<Token>>, name: &str) -> Result<Vec<Node>, String> {
    let mut nodes = Vec::new();
    while let Some(token) = tokens.peek().cloned() {
        match token {
            Token::Text(text) => {
                tokens.next();
                nodes.push(Node::Text(text));
            }
            Token::Action(action) if action == "end" || action == "else" => return Ok(nodes),
            Token::Action(action) => {
                tokens.next();
                if let Some(node) = parse_action(&action, tokens, name)? {
                    nodes.push(node);
                }
            }
        }
    }
    Ok(nodes)
}

fn parse_action(
    action: &str,
    tokens: &mut std::iter::Peekable<std::vec::IntoIter<Token>>,
    name: &str,
) -> Result<Option<Node>, String> {
    if action.starts_with("/*") {
        return Ok(None);
    }
    if let Some(condition) = action.strip_prefix("if ") {
        return parse_if(condition, tokens, name).map(Some);
    }
    if let Some(arguments) = action.strip_prefix(".WriteFile ") {
        return parse_write_file(arguments, name).map(Some);
    }
    match action.strip_prefix('.') {
        Some(field) => Ok(Some(Node::Field(field.to_owned()))),
        None => Err(format!("template {name:?}: unsupported action {action:?}")),
    }
}

fn parse_if(
    condition: &str,
    tokens: &mut std::iter::Peekable<std::vec::IntoIter<Token>>,
    name: &str,
) -> Result<Node, String> {
    let condition = parse_condition(condition, name)?;
    let then = parse(tokens, name)?;
    let otherwise = match tokens.next() {
        Some(Token::Action(action)) if action == "else" => {
            let otherwise = parse(tokens, name)?;
            tokens.next();
            otherwise
        }
        _ => Vec::new(),
    };
    Ok(Node::If {
        condition,
        then,
        otherwise,
    })
}

fn parse_condition(condition: &str, name: &str) -> Result<Condition, String> {
    let words: Vec<&str> = condition.split_whitespace().collect();
    match words.as_slice() {
        [field] => field
            .strip_prefix('.')
            .map(|field| Condition::Field(field.to_owned()))
            .ok_or_else(|| format!("template {name:?}: unsupported condition {condition:?}")),
        ["eq", field, literal] => Ok(Condition::Equals {
            field: field.trim_start_matches('.').to_owned(),
            literal: literal.trim_matches('"').to_owned(),
        }),
        _ => Err(format!("template {name:?}: unsupported condition {condition:?}")),
    }
}

fn parse_write_file(arguments: &str, name: &str) -> Result<Node, String> {
    let (path, mode) = arguments
        .rsplit_once(' ')
        .ok_or_else(|| format!("template {name:?}: WriteFile needs a path and a mode"))?;
    let mode = mode
        .strip_prefix("0o")
        .and_then(|digits| u32::from_str_radix(digits, 8).ok())
        .ok_or_else(|| format!("template {name:?}: bad WriteFile mode {mode:?}"))?;
    Ok(Node::WriteFile(WrittenFile {
        path: path.trim().trim_matches('"').to_owned(),
        mode,
    }))
}

fn execute(nodes: &[Node], values: &Values, out: &mut Rendered) -> Result<(), String> {
    nodes.iter().try_for_each(|node| match node {
        Node::Text(text) => {
            out.text.push_str(text);
            Ok(())
        }
        Node::Field(field) => {
            out.text.push_str(&value(values, field)?.render());
            Ok(())
        }
        Node::WriteFile(file) => {
            out.files.push(file.clone());
            Ok(())
        }
        Node::If {
            condition,
            then,
            otherwise,
        } => match holds(condition, values)? {
            true => execute(then, values, out),
            false => execute(otherwise, values, out),
        },
    })
}

fn holds(condition: &Condition, values: &Values) -> Result<bool, String> {
    match condition {
        Condition::Field(field) => value(values, field).map(Value::truthy),
        Condition::Equals { field, literal } => value(values, field).map(|value| value.render() == *literal),
    }
}

fn value<'a>(values: &'a Values, field: &str) -> Result<&'a Value, String> {
    values.get(field).ok_or_else(|| format!("can't evaluate field {field}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values() -> Values {
        Values::from([
            ("Name", Value::Text("e2b".to_owned())),
            ("On", Value::Flag(true)),
            ("Off", Value::Flag(false)),
            ("Limit", Value::Number(50)),
        ])
    }

    #[test]
    fn renders_fields_files_and_trim_markers() {
        let source = "{{- /*gotype:x*/ -}}\n{{ .WriteFile \"/etc/probe\" 0o644 }}\nhost {{ .Name }} {{ .Limit }}\n{{- if .On }}\non\n{{- else }}\noff\n{{- end }}\n";

        let rendered = render("probe", source, &values()).unwrap();

        assert_eq!(rendered.text, "\nhost e2b 50\non\n");
        assert_eq!(
            rendered.files,
            vec![WrittenFile {
                path: "/etc/probe".to_owned(),
                mode: 0o644
            }]
        );
    }

    #[test]
    fn false_branches_and_eq_conditions() {
        let source = "{{- if .Off -}}\nhidden\n{{- end -}}{{ if eq .Name \"e2b\" }}yes{{ end }}";

        assert_eq!(render("x", source, &values()).unwrap().text, "yes");
    }

    #[test]
    fn unknown_fields_fail_like_go() {
        assert_eq!(
            render("x", "{{ .Missing }}", &values()).unwrap_err(),
            "error executing template \"x\": can't evaluate field Missing"
        );
    }
}
