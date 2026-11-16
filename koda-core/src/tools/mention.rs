//! `@` mentions, read by the host under the Read tool's rules, as Claude
//! Code ties `@` to its Read tool: the same path check, the same file
//! system (the sandbox's), the same limit and "continue" note, and the same
//! refusal of a binary file. One rule for both, so a mention can't reach
//! what Read can't, and a client on another machine names files on the
//! host's.
//!
//! A directory gives the List tool's listing; an image is sent as an image.
//! What Read refuses comes back as
//! [`Mentioned::Unread`](crate::tools::mention::Mentioned::Unread), and the model gets
//! the path and why, as Claude Code sends only the path for a file its Read
//! refuses.

use serde_json::json;

use super::{ToolRegistry, file_tools};
use crate::providers::ImageData;

/// What a mentioned path reads as.
#[derive(Debug, Clone)]
pub enum Mentioned {
    /// A text file: what Read returns for it, numbered and cut at the
    /// limit with how to read on.
    File(String),
    /// A directory: what List returns for it.
    Directory(String),
    /// An image, sent to the model as one; or a PDF of at most 10 pages,
    /// sent whole (to a model that doesn't take PDFs, as a note).
    Image(ImageData),
    /// Not read: a binary file, a path that doesn't exist or can't be read,
    /// or Koda's own secrets. Why, for the model.
    Unread(String),
}

impl ToolRegistry {
    /// Read the mentioned `path`, relative to the project or absolute.
    pub async fn read_mention(&self, path: &str) -> Mentioned {
        let resolved = match super::resolve_read_path(&self.project_root, path) {
            Ok(resolved) => resolved,
            Err(e) => return Mentioned::Unread(e.to_string()),
        };
        let metadata = match tokio::fs::metadata(&resolved).await {
            Ok(metadata) => metadata,
            Err(e) => return Mentioned::Unread(format!("{path} can't be read: {e}")),
        };
        if metadata.is_dir() {
            return match file_tools::list_files(
                &self.project_root,
                &json!({"path": path}),
                self.caps.list_entries,
            )
            .await
            {
                Ok(listing) => Mentioned::Directory(listing),
                Err(e) => Mentioned::Unread(e.to_string()),
            };
        }
        if let Ok(bytes) = self.fs.read(&resolved, None).await
            && let Some(media_type) = file_tools::image_media_type(&bytes)
        {
            // Scaled and recompressed to the model's limits, as Read does.
            return match super::media::prepare_image(bytes, media_type) {
                Ok(prepared) => Mentioned::Image(prepared.image),
                Err(e) => Mentioned::Unread(format!("{path} can't be read as an image: {e}")),
            };
        }
        if let Ok(bytes) = self.fs.read(&resolved, None).await
            && bytes.starts_with(b"%PDF-")
        {
            return self.mention_pdf(path, bytes).await;
        }
        match file_tools::read_file(
            &self.project_root,
            &json!({"file_path": path}),
            &self.read_cache,
            &*self.fs,
            self.caps.tool_result_chars,
        )
        .await
        {
            Ok(text) => Mentioned::File(text),
            Err(e) => Mentioned::Unread(e.to_string()),
        }
    }
}

impl ToolRegistry {
    /// A mentioned PDF: whole if it has at most 10 pages (or poppler can't
    /// count them) and fits, as Read returns one; else the path, with how
    /// to read it in ranges.
    async fn mention_pdf(&self, path: &str, bytes: Vec<u8>) -> Mentioned {
        use super::pdf::{MAX_WHOLE_PAGES, MAX_WHOLE_PDF_BYTES, page_count};
        match page_count(&bytes).await {
            Ok(Some(n)) if n > MAX_WHOLE_PAGES => Mentioned::Unread(format!(
                "{path} is a PDF of {n} pages; read it in ranges with Read's pages, such as \
                 \"1-10\"."
            )),
            Err(e) => Mentioned::Unread(format!("{path} can't be read as a PDF: {e}")),
            Ok(_) if bytes.len() > MAX_WHOLE_PDF_BYTES => Mentioned::Unread(format!(
                "{path} is a PDF too large to send whole; read it in ranges with Read's pages."
            )),
            Ok(_) => Mentioned::Image(ImageData {
                media_type: crate::providers::PDF_MEDIA_TYPE.to_string(),
                base64: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes),
            }),
        }
    }
}

/// What the model is sent for `mentions`, after the user's text: each file
/// and directory in a block of its own, and each path that wasn't read with
/// why, so the model can read it another way. Images go with the message as
/// images, not here. `None` when there is nothing to add.
pub fn mention_context(mentions: &[(String, Mentioned)]) -> Option<String> {
    let blocks: Vec<String> = mentions
        .iter()
        .filter_map(|(path, mentioned)| match mentioned {
            Mentioned::File(text) => Some(format!("<file path=\"{path}\">\n{text}\n</file>")),
            Mentioned::Directory(listing) => Some(format!(
                "<directory path=\"{path}\">\n{listing}\n</directory>"
            )),
            Mentioned::Unread(why) => Some(format!(
                "<file path=\"{path}\" unread=\"true\">\n{why}\n</file>"
            )),
            Mentioned::Image(_) => None,
        })
        .collect();
    (!blocks.is_empty()).then(|| blocks.join("\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry(root: &std::path::Path) -> ToolRegistry {
        ToolRegistry::new(root.to_path_buf(), 100_000)
    }

    #[tokio::test]
    async fn a_text_file_reads_as_read_returns_it() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        let Mentioned::File(text) = registry(dir.path()).read_mention("a.txt").await else {
            panic!("not read as a file");
        };
        assert_eq!(text, "     1\tone\n     2\ttwo");
    }

    /// A notebook mentioned reads as Read shows it: its cells, not its JSON.
    #[tokio::test]
    async fn a_notebook_reads_as_its_cells() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("nb.ipynb"),
            r#"{"nbformat": 4, "cells": [{"cell_type": "code", "source": ["x = 1"], "outputs": []}]}"#,
        )
        .unwrap();
        let Mentioned::File(text) = registry(dir.path()).read_mention("nb.ipynb").await else {
            panic!("not read as a file");
        };
        assert!(
            text.contains("<cell index=\"1\" type=\"code\">\nx = 1\n</cell>"),
            "{text}"
        );
    }

    /// A short PDF mentioned is sent whole; a long one gives the path and
    /// how to read it in ranges.
    #[tokio::test]
    async fn a_short_pdf_is_sent_whole_and_a_long_one_by_path() {
        if !crate::tools::pdf::tests::has_poppler() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.pdf"),
            crate::tools::pdf::tests::sample_pdf(2, ""),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("b.pdf"),
            crate::tools::pdf::tests::sample_pdf(12, ""),
        )
        .unwrap();
        let registry = registry(dir.path());
        let Mentioned::Image(pdf) = registry.read_mention("a.pdf").await else {
            panic!("a short PDF wasn't sent whole");
        };
        assert_eq!(pdf.media_type, crate::providers::PDF_MEDIA_TYPE);
        let Mentioned::Unread(why) = registry.read_mention("b.pdf").await else {
            panic!("a long PDF was sent");
        };
        assert!(why.contains("12 pages") && why.contains("1-10"), "{why}");
    }

    #[tokio::test]
    async fn a_directory_gives_a_listing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "").unwrap();
        let Mentioned::Directory(listing) = registry(dir.path()).read_mention("src").await else {
            panic!("not read as a directory");
        };
        assert!(listing.contains("lib.rs"), "{listing}");
    }

    #[tokio::test]
    async fn an_image_is_sent_as_one() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("shot.dat"), b"\x89\x50\x4e\x47\x0d\x0a\x1a\x0a\x00\x00\x00\x0d\x49\x48\x44\x52\x00\x00\x00\x01\x00\x00\x00\x01\x08\x02\x00\x00\x00\x90\x77\x53\xde\x00\x00\x00\x0c\x49\x44\x41\x54\x78\x9c\x63\xb8\x23\x22\x02\x00\x02\xd4\x01\x05\x59\x70\xd3\xc2\x00\x00\x00\x00\x49\x45\x4e\x44\xae\x42\x60\x82").unwrap();
        let Mentioned::Image(image) = registry(dir.path()).read_mention("shot.dat").await else {
            panic!("not read as an image");
        };
        assert_eq!(image.media_type, "image/png");
    }

    #[tokio::test]
    async fn a_binary_or_missing_file_is_not_read_and_says_why() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("data.bin"), b"abc\0def").unwrap();
        let registry = registry(dir.path());
        let Mentioned::Unread(why) = registry.read_mention("data.bin").await else {
            panic!("a binary file was read");
        };
        assert!(why.contains("a binary file"), "{why}");
        let Mentioned::Unread(why) = registry.read_mention("nope.txt").await else {
            panic!("a missing file was read");
        };
        assert!(why.contains("nope.txt"), "{why}");
    }

    /// Outside the project is fine, as for Read; Koda's own secrets aren't.
    #[tokio::test]
    async fn the_path_rule_is_reads() {
        let project = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let outside = elsewhere.path().join("notes.txt");
        std::fs::write(&outside, "hello").unwrap();
        let mentioned = registry(project.path())
            .read_mention(outside.to_str().unwrap())
            .await;
        assert!(matches!(mentioned, Mentioned::File(_)), "{mentioned:?}");
    }

    #[test]
    fn the_context_has_a_block_per_file_and_skips_images() {
        let context = mention_context(&[
            ("a.txt".into(), Mentioned::File("     1\thi".into())),
            (
                "gone.txt".into(),
                Mentioned::Unread("gone.txt can't be read".into()),
            ),
            (
                "x.png".into(),
                Mentioned::Image(ImageData {
                    media_type: "image/png".into(),
                    base64: String::new(),
                }),
            ),
        ])
        .unwrap();
        assert!(context.contains("<file path=\"a.txt\">\n     1\thi\n</file>"));
        assert!(context.contains("unread=\"true\""));
        assert!(!context.contains("x.png"));
        assert!(mention_context(&[]).is_none());
    }
}
