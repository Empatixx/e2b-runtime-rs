pub const COPY_SCRIPT: &str = include_str!("../../../assets/template/commands/copy_script.sh");
pub const WORKDIR_SCRIPT: &str = include_str!("../../../assets/template/commands/workdir.sh");
pub const ADMIN_GROUP_SCRIPT: &str = include_str!("../../../assets/template/commands/admin_group.sh");

const OPEN: &str = "{{";
const CLOSE: &str = "}}";

pub fn render(template: &str, fields: &[(&str, &str)]) -> Result<String, String> {
    let mut rendered = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find(OPEN) {
        let end = rest[start..]
            .find(CLOSE)
            .map(|end| start + end)
            .ok_or_else(|| "unclosed action".to_owned())?;
        rendered.push_str(&rest[..start]);
        rendered.push_str(field(fields, &rest[start + OPEN.len()..end])?);
        rest = &rest[end + CLOSE.len()..];
    }
    rendered.push_str(rest);
    Ok(rendered)
}

fn field<'a>(fields: &[(&str, &'a str)], action: &str) -> Result<&'a str, String> {
    let name = action.trim().trim_start_matches('.');
    fields
        .iter()
        .find(|(key, _)| *key == name)
        .map(|(_, value)| *value)
        .ok_or_else(|| format!("can't evaluate field {name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_substituted_once_without_escaping() {
        assert_eq!(
            render("a=\"{{ .A }}\" b={{.B}}", &[("A", "{{ .B }} \"q\""), ("B", "$(x)")]),
            Ok("a=\"{{ .B }} \"q\"\" b=$(x)".to_owned())
        );
        assert_eq!(
            render("{{ .Missing }}", &[]),
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
