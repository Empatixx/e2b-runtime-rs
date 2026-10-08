use std::collections::BTreeMap;

pub const COPY_SCRIPT: &str = include_str!("../../../assets/template/commands/copy_script.sh");
pub const WORKDIR_SCRIPT: &str = include_str!("../../../assets/template/commands/workdir.sh");
pub const ADMIN_GROUP_SCRIPT: &str = include_str!("../../../assets/template/commands/admin_group.sh");

const OPEN: &str = "{{";
const CLOSE: &str = "}}";

pub struct Script<'a> {
    template: &'a str,
    values: BTreeMap<&'a str, &'a str>,
}

impl<'a> Script<'a> {
    pub fn new(template: &'a str) -> Self {
        Self {
            template,
            values: BTreeMap::new(),
        }
    }

    pub fn value(mut self, name: &'a str, value: &'a str) -> Self {
        self.values.insert(name, value);
        self
    }

    pub fn render(&self) -> Result<String, String> {
        let mut rendered = String::with_capacity(self.template.len());
        let mut rest = self.template;
        while let Some(start) = rest.find(OPEN) {
            let end = rest[start..]
                .find(CLOSE)
                .map(|end| start + end)
                .ok_or_else(|| "unclosed action".to_owned())?;
            rendered.push_str(&rest[..start]);
            rendered.push_str(self.field(&rest[start + OPEN.len()..end])?);
            rest = &rest[end + CLOSE.len()..];
        }
        rendered.push_str(rest);
        Ok(rendered)
    }

    fn field(&self, action: &str) -> Result<&'a str, String> {
        let name = action.trim().trim_start_matches('.');
        self.values
            .get(name)
            .copied()
            .ok_or_else(|| format!("can't evaluate field {name}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_substituted_once_without_escaping() {
        assert_eq!(
            Script::new("a=\"{{ .A }}\" b={{.B}}")
                .value("A", "{{ .B }} \"q\"")
                .value("B", "$(x)")
                .render(),
            Ok("a=\"{{ .B }} \"q\"\" b=$(x)".to_owned())
        );
        assert_eq!(
            Script::new("{{ .Missing }}").render(),
            Err("can't evaluate field Missing".to_owned())
        );
    }

    #[test]
    fn the_copy_script_is_the_upstream_file() {
        assert!(COPY_SCRIPT.starts_with("#!/bin/bash\n\nset -o pipefail\n\ntargetPath=\"{{ .TargetPath }}\"\n"));
        assert!(
            COPY_SCRIPT
                .ends_with("    echo \"Error: entry is neither file, directory, nor symlink\"\n    exit 1\nfi\n")
        );
        assert_eq!(COPY_SCRIPT.lines().count(), 92);
    }
}
