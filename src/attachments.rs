use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use base64::Engine;

use crate::context::ImageAttachment;

pub const MAX_ATTACHMENTS: usize = 8;
pub const MAX_IMAGE_ATTACHMENTS: usize = 4;
pub const MAX_IMAGE_BYTES: u64 = 5 * 1_024 * 1_024;
pub const MAX_TEXT_BYTES: u64 = 128 * 1_024;
pub const MAX_TOTAL_BYTES: u64 = 16 * 1_024 * 1_024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PendingAttachment {
    Image {
        path: PathBuf,
        attachment: ImageAttachment,
        bytes: u64,
    },
    Text {
        path: PathBuf,
        content: String,
        bytes: u64,
    },
}

impl PendingAttachment {
    pub fn path(&self) -> &Path {
        match self {
            Self::Image { path, .. } | Self::Text { path, .. } => path,
        }
    }

    pub fn display_name(&self) -> String {
        let kind = match self {
            Self::Image { .. } => "image",
            Self::Text { .. } => "file",
        };
        format!("{kind}:{}", compact_path(self.path()))
    }

    fn bytes(&self) -> u64 {
        match self {
            Self::Image { bytes, .. } | Self::Text { bytes, .. } => *bytes,
        }
    }
}

pub fn load(path: &Path, base: &Path) -> Result<PendingAttachment> {
    let path = resolve_path(path, base)?;
    let metadata = std::fs::metadata(&path)
        .with_context(|| format!("failed to inspect attachment {}", path.display()))?;
    if !metadata.is_file() {
        bail!("attachment {} is not a regular file", path.display());
    }
    if metadata.len() == 0 {
        bail!("attachment {} is empty", path.display());
    }
    if metadata.len() > MAX_IMAGE_BYTES.max(MAX_TEXT_BYTES) {
        bail!(
            "attachment {} is too large ({} bytes); maximum image size is {} bytes",
            path.display(),
            metadata.len(),
            MAX_IMAGE_BYTES
        );
    }
    let data = std::fs::read(&path)
        .with_context(|| format!("failed to read attachment {}", path.display()))?;
    if let Some(media_type) = detect_image_media_type(&data) {
        if metadata.len() > MAX_IMAGE_BYTES {
            bail!(
                "image attachment {} exceeds the {} byte limit",
                path.display(),
                MAX_IMAGE_BYTES
            );
        }
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("image")
            .chars()
            .take(240)
            .collect();
        return Ok(PendingAttachment::Image {
            path,
            attachment: ImageAttachment {
                media_type: media_type.into(),
                data: base64::engine::general_purpose::STANDARD.encode(data),
                name,
            },
            bytes: metadata.len(),
        });
    }
    if metadata.len() > MAX_TEXT_BYTES {
        bail!(
            "text attachment {} exceeds the {} byte limit",
            path.display(),
            MAX_TEXT_BYTES
        );
    }
    if data.contains(&0) {
        bail!(
            "attachment {} is binary but not a supported PNG, JPEG, GIF, or WebP image",
            path.display()
        );
    }
    let content = String::from_utf8(data)
        .with_context(|| format!("text attachment {} is not valid UTF-8", path.display()))?;
    Ok(PendingAttachment::Text {
        path,
        content,
        bytes: metadata.len(),
    })
}

pub fn validate_set(attachments: &[PendingAttachment]) -> Result<()> {
    if attachments.len() > MAX_ATTACHMENTS {
        bail!("at most {MAX_ATTACHMENTS} files may be attached to one message");
    }
    let images = attachments
        .iter()
        .filter(|attachment| matches!(attachment, PendingAttachment::Image { .. }))
        .count();
    if images > MAX_IMAGE_ATTACHMENTS {
        bail!("at most {MAX_IMAGE_ATTACHMENTS} images may be attached to one message");
    }
    let total = attachments
        .iter()
        .map(PendingAttachment::bytes)
        .fold(0_u64, u64::saturating_add);
    if total > MAX_TOTAL_BYTES {
        bail!("attachments total {total} bytes; maximum per message is {MAX_TOTAL_BYTES} bytes");
    }
    Ok(())
}

pub fn prepare_message(
    text: &str,
    attachments: Vec<PendingAttachment>,
) -> Result<(String, Vec<ImageAttachment>)> {
    validate_set(&attachments)?;
    if attachments.is_empty() {
        return Ok((text.to_owned(), Vec::new()));
    }
    let mut prompt = String::from(text);
    let mut images = Vec::new();
    for attachment in attachments {
        match attachment {
            PendingAttachment::Image {
                path, attachment, ..
            } => {
                prompt.push_str("\n\n<attached_image name=\"");
                prompt.push_str(&escape_attribute(&compact_path(&path)));
                prompt.push_str("\" />");
                images.push(attachment);
            }
            PendingAttachment::Text { path, content, .. } => {
                prompt.push_str("\n\n<attached_file name=\"");
                prompt.push_str(&escape_attribute(&compact_path(&path)));
                prompt.push_str("\">\n");
                prompt.push_str(&content);
                if !content.ends_with('\n') {
                    prompt.push('\n');
                }
                prompt.push_str("</attached_file>");
            }
        }
    }
    Ok((prompt, images))
}

pub fn resolve_file_mentions(text: &str, base: &Path) -> Result<(String, Vec<PendingAttachment>)> {
    let mut output = String::with_capacity(text.len());
    let mut attachments = Vec::new();
    let mut copied_until = 0;
    let mut indices = text.char_indices().peekable();

    while let Some((start, character)) = indices.next() {
        if character != '@' || !starts_file_mention(text, start) {
            continue;
        }
        let Some((next, next_character)) = indices.peek().copied() else {
            continue;
        };
        if next_character == '@' {
            continue;
        }

        let parsed = if matches!(next_character, '"' | '\'') {
            parse_quoted_mention(text, next, next_character).map(|value| (value, true))
        } else {
            parse_plain_mention(text, next).map(|value| (value, false))
        };
        let Some(((path_text, token_end, path_end), quoted)) = parsed else {
            continue;
        };
        let Some((resolved_text, resolved_path_end)) =
            existing_mention_path(path_text, path_end, base)
        else {
            continue;
        };
        let attachment = load(Path::new(resolved_text), base)?;
        if !attachments
            .iter()
            .any(|current: &PendingAttachment| current.path() == attachment.path())
        {
            attachments.push(attachment);
            validate_set(&attachments)?;
        }

        output.push_str(&text[copied_until..start]);
        output.push('`');
        output.push_str(resolved_text);
        output.push('`');
        if !quoted {
            output.push_str(&text[resolved_path_end..token_end]);
        }
        copied_until = token_end;
        while indices.peek().is_some_and(|(index, _)| *index < token_end) {
            indices.next();
        }
    }

    if attachments.is_empty() {
        return Ok((text.to_owned(), attachments));
    }
    output.push_str(&text[copied_until..]);
    Ok((output, attachments))
}

pub fn looks_like_image_path(value: &str) -> bool {
    let value = strip_quotes(value.trim());
    if value.is_empty() || value.contains('\n') || value.contains('\r') || value.contains("://") {
        return false;
    }
    let lower = value.to_ascii_lowercase();
    [".png", ".jpg", ".jpeg", ".gif", ".webp"]
        .iter()
        .any(|extension| lower.ends_with(extension))
}

pub fn normalized_path_text(value: &str) -> &str {
    strip_quotes(value.trim())
}

pub(crate) fn starts_file_mention(text: &str, start: usize) -> bool {
    text[..start]
        .chars()
        .next_back()
        .is_none_or(|character| character.is_whitespace() || "([{".contains(character))
}

fn parse_quoted_mention(
    text: &str,
    quote_start: usize,
    quote: char,
) -> Option<(&str, usize, usize)> {
    let path_start = quote_start + quote.len_utf8();
    let relative_end = text[path_start..].find(quote)?;
    let path_end = path_start + relative_end;
    let token_end = path_end + quote.len_utf8();
    (path_end > path_start).then_some((&text[path_start..path_end], token_end, path_end))
}

fn parse_plain_mention(text: &str, path_start: usize) -> Option<(&str, usize, usize)> {
    let token_end = text[path_start..]
        .char_indices()
        .find_map(|(offset, character)| character.is_whitespace().then_some(path_start + offset))
        .unwrap_or(text.len());
    (token_end > path_start).then_some((&text[path_start..token_end], token_end, token_end))
}

fn existing_mention_path<'a>(
    path_text: &'a str,
    absolute_end: usize,
    base: &Path,
) -> Option<(&'a str, usize)> {
    let mut candidate = path_text;
    loop {
        if resolve_path(Path::new(candidate), base).is_ok() {
            return Some((
                candidate,
                absolute_end - (path_text.len() - candidate.len()),
            ));
        }
        let last = candidate.chars().next_back()?;
        if !",.;:!?)]}".contains(last) {
            return None;
        }
        candidate = &candidate[..candidate.len() - last.len_utf8()];
    }
}

fn resolve_path(path: &Path, base: &Path) -> Result<PathBuf> {
    let display = path.to_string_lossy();
    let expanded = if display == "~" {
        directories::BaseDirs::new()
            .map(|dirs| dirs.home_dir().to_path_buf())
            .unwrap_or_else(|| path.to_path_buf())
    } else if let Some(suffix) = display.strip_prefix("~/") {
        directories::BaseDirs::new()
            .map(|dirs| dirs.home_dir().join(suffix))
            .unwrap_or_else(|| path.to_path_buf())
    } else {
        path.to_path_buf()
    };
    let candidate = if expanded.is_absolute() {
        expanded
    } else {
        base.join(expanded)
    };
    candidate
        .canonicalize()
        .with_context(|| format!("attachment {} does not exist", candidate.display()))
}

fn detect_image_media_type(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if data.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if data.len() >= 12 && data.starts_with(b"RIFF") && &data[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

fn strip_quotes(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .or_else(|| {
            value
                .strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
        })
        .unwrap_or(value)
}

fn escape_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn compact_path(path: &Path) -> String {
    let display = path.display().to_string();
    let Some(home) = directories::BaseDirs::new().map(|dirs| dirs.home_dir().display().to_string())
    else {
        return display;
    };
    display
        .strip_prefix(&home)
        .map(|suffix| format!("~{suffix}"))
        .unwrap_or(display)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_text_and_magic_checked_images() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("note.txt"), "hello <world>").unwrap();
        std::fs::write(temp.path().join("screen.bin"), b"\x89PNG\r\n\x1a\nfixture").unwrap();

        let text = load(Path::new("note.txt"), temp.path()).unwrap();
        let image = load(Path::new("screen.bin"), temp.path()).unwrap();
        assert!(matches!(text, PendingAttachment::Text { .. }));
        assert!(matches!(image, PendingAttachment::Image { .. }));

        let (prompt, images) = prepare_message("inspect these", vec![text, image]).unwrap();
        assert!(prompt.contains("<attached_file"));
        assert!(prompt.contains("hello <world>"));
        assert!(prompt.contains("<attached_image"));
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].media_type, "image/png");
    }

    #[test]
    fn rejects_binary_unknown_images_and_bounds_attachment_sets() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("unknown.bin"), b"a\0b").unwrap();
        assert!(load(Path::new("unknown.bin"), temp.path()).is_err());

        let attachment = PendingAttachment::Text {
            path: temp.path().join("small.txt"),
            content: "x".into(),
            bytes: 1,
        };
        assert!(validate_set(&vec![attachment; MAX_ATTACHMENTS + 1]).is_err());
    }

    #[test]
    fn recognizes_pasted_image_paths_without_matching_sentences() {
        assert!(looks_like_image_path("\"/tmp/my screen.PNG\""));
        assert!(looks_like_image_path("C:\\Users\\me\\screen.webp"));
        assert!(!looks_like_image_path("please inspect image.png now"));
        assert!(!looks_like_image_path("https://example.com/image.png"));
        assert!(!looks_like_image_path("image.svg"));
    }

    #[test]
    fn resolves_existing_file_mentions_and_preserves_regular_at_text() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("src")).unwrap();
        std::fs::write(temp.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(
            temp.path().join("failure image.png"),
            b"\x89PNG\r\n\x1a\nfixture",
        )
        .unwrap();

        let (text, attachments) = resolve_file_mentions(
            "Fix @src/main.rs, compare @\"failure image.png\", email a@b.com and keep @missing.",
            temp.path(),
        )
        .unwrap();

        assert_eq!(
            text,
            "Fix `src/main.rs`, compare `failure image.png`, email a@b.com and keep @missing."
        );
        assert_eq!(attachments.len(), 2);
        assert!(matches!(attachments[0], PendingAttachment::Text { .. }));
        assert!(matches!(attachments[1], PendingAttachment::Image { .. }));
    }

    #[test]
    fn deduplicates_repeated_file_mentions() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("note.txt"), "hello").unwrap();

        let (_, attachments) =
            resolve_file_mentions("@note.txt then @./note.txt", temp.path()).unwrap();
        assert_eq!(attachments.len(), 1);
    }
}
