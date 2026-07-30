// Adapted from OpenAI Codex's core-skills frontmatter loader.
// Copyright 2025 OpenAI. Licensed under Apache-2.0.
// Modified for WeCode: exposes a small string-field map shared by commands and skills.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use serde_yaml::Value;

pub(crate) fn parse_string_fields(frontmatter: &str) -> Result<BTreeMap<String, String>> {
    if frontmatter.trim().is_empty() {
        return Ok(BTreeMap::new());
    }
    let parsed: Value = match serde_yaml::from_str(frontmatter) {
        Ok(parsed) => Ok(parsed),
        Err(original_error) => match repair_scalar_fields(frontmatter) {
            Some(repaired) => serde_yaml::from_str(&repaired).map_err(|_| original_error),
            None => Err(original_error),
        },
    }
    .context("invalid YAML frontmatter")?;
    let mapping = parsed
        .as_mapping()
        .context("frontmatter must be a YAML mapping")?;
    let mut fields = BTreeMap::new();
    for (key, value) in mapping {
        let Some(key) = key.as_str() else {
            bail!("frontmatter keys must be strings");
        };
        let value = match value {
            Value::Null => String::new(),
            Value::Bool(value) => value.to_string(),
            Value::Number(value) => value.to_string(),
            Value::String(value) => value.clone(),
            Value::Sequence(_) | Value::Mapping(_) | Value::Tagged(_) => continue,
        };
        fields.insert(key.trim().to_ascii_lowercase(), value);
    }
    Ok(fields)
}

fn repair_scalar_fields(frontmatter: &str) -> Option<String> {
    let mut changed = false;
    let mut block_scalar_indent = None;
    let mut repaired = Vec::new();
    for line in frontmatter.lines() {
        let indent = line
            .chars()
            .take_while(|character| *character == ' ')
            .count();
        if let Some(block_indent) = block_scalar_indent {
            if line.trim().is_empty() || indent > block_indent {
                repaired.push(line.to_owned());
                continue;
            }
            block_scalar_indent = None;
        }

        let Some((key, value)) = line.split_once(':') else {
            repaired.push(line.to_owned());
            continue;
        };
        if key.trim().is_empty() || !value.chars().next().is_none_or(char::is_whitespace) {
            repaired.push(line.to_owned());
            continue;
        }

        let trimmed_start = value.trim_start();
        let leading_whitespace = &value[..value.len() - trimmed_start.len()];
        let mut scalar = trimmed_start;
        let mut comment = "";
        for (index, character) in trimmed_start.char_indices() {
            if character == '#'
                && (index == 0
                    || trimmed_start[..index]
                        .chars()
                        .next_back()
                        .is_some_and(char::is_whitespace))
            {
                let comment_start = trimmed_start[..index].trim_end().len();
                scalar = &trimmed_start[..comment_start];
                comment = &trimmed_start[comment_start..];
                break;
            }
        }

        let scalar = scalar.trim_end();
        let Some(first_character) = scalar.chars().next() else {
            repaired.push(line.to_owned());
            continue;
        };
        if matches!(first_character, '|' | '>') {
            block_scalar_indent = Some(indent);
            repaired.push(line.to_owned());
            continue;
        }
        if matches!(first_character, '\'' | '"') {
            repaired.push(line.to_owned());
            continue;
        }
        let has_colon_separator = scalar.char_indices().any(|(index, character)| {
            character == ':'
                && scalar[index + character.len_utf8()..]
                    .chars()
                    .next()
                    .is_some_and(char::is_whitespace)
        });
        let invalid_flow_scalar = matches!(first_character, '[' | '{' | '@' | '`')
            && serde_yaml::from_str::<Value>(scalar).is_err();
        if !has_colon_separator && !invalid_flow_scalar {
            repaired.push(line.to_owned());
            continue;
        }

        repaired.push(format!(
            "{key}:{leading_whitespace}'{}'{comment}",
            scalar.replace('\'', "''")
        ));
        changed = true;
    }
    changed.then(|| repaired.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::parse_string_fields;

    #[test]
    fn parses_yaml_scalars_and_block_text() {
        let fields = parse_string_fields(
            "name: deploy\ndescription: >\n  Deploy the service\n  safely\ndisabled: true",
        )
        .unwrap();
        assert_eq!(fields["name"], "deploy");
        assert_eq!(fields["description"], "Deploy the service safely\n");
        assert_eq!(fields["disabled"], "true");
    }

    #[test]
    fn repairs_common_unquoted_prose() {
        let fields =
            parse_string_fields("description: Build for AWS: ECS\nargument-hint: <env: prod>")
                .unwrap();
        assert_eq!(fields["description"], "Build for AWS: ECS");
        assert_eq!(fields["argument-hint"], "<env: prod>");
    }
}
