//! What Read returns for a PDF, as Claude Code's Read does: a short PDF
//! (at most [`MAX_WHOLE_PAGES`](crate::tools::pdf::MAX_WHOLE_PAGES) pages)
//! whole, for a model that takes PDFs; a longer one in ranges given by
//! `pages` (at most [`MAX_PAGES_PER_READ`](crate::tools::pdf::MAX_PAGES_PER_READ)
//! at a time), as page images. A model that takes images but not PDFs gets
//! page images too, and a text-only model the pages' text, as qwen-code's
//! Read falls back to.
//!
//! Pages are counted, drawn and read by poppler's `pdfinfo`, `pdftoppm`
//! and `pdftotext`, as Claude Code draws them with `pdftoppm`, rather than
//! by a PDF library in Koda. Without poppler a short PDF still reaches a
//! model that takes PDFs whole; anything else says how to install it. Each
//! tool runs on a private copy of the bytes Read already read, so it sees
//! the file as Read's file system does.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

use super::media::{MAX_EDGE, PreparedImage};

/// The most pages Read returns without `pages`, as Claude Code's Read.
pub const MAX_WHOLE_PAGES: u32 = 10;

/// The most pages one read with `pages` returns, as Claude Code's Read.
pub const MAX_PAGES_PER_READ: u32 = 20;

/// The largest PDF sent whole: well within Anthropic's 128 MB request once
/// base64-encoded. A larger one is read in ranges.
pub const MAX_WHOLE_PDF_BYTES: usize = 32 * 1024 * 1024;

/// How long one poppler tool may run.
const TOOL_TIMEOUT: Duration = Duration::from_secs(60);

/// Pages `first` to `last`, 1-based and inclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageRange {
    /// The first page.
    pub first: u32,
    /// The last page.
    pub last: u32,
}

impl PageRange {
    /// How many pages the range holds.
    pub fn len(&self) -> u32 {
        self.last - self.first + 1
    }

    /// Never empty; for clippy's `len_without_is_empty`.
    pub fn is_empty(&self) -> bool {
        false
    }
}

impl std::fmt::Display for PageRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.first == self.last {
            write!(f, "page {}", self.first)
        } else {
            write!(f, "pages {}-{}", self.first, self.last)
        }
    }
}

/// `pages` as Read takes it: `"3"` or `"1-5"`, 1-based, at most
/// [`MAX_PAGES_PER_READ`] pages.
pub fn parse_pages(pages: &str) -> Result<PageRange> {
    let pages = pages.trim();
    let number = |s: &str| -> Result<u32> {
        s.trim()
            .parse::<u32>()
            .ok()
            .filter(|n| *n >= 1)
            .ok_or_else(|| {
                anyhow!("pages must be a page number or a range such as \"1-5\", not {pages:?}")
            })
    };
    let range = match pages.split_once('-') {
        Some((first, last)) => PageRange {
            first: number(first)?,
            last: number(last)?,
        },
        None => {
            let page = number(pages)?;
            PageRange {
                first: page,
                last: page,
            }
        }
    };
    if range.last < range.first {
        bail!("pages {pages:?} ends before it starts");
    }
    if range.len() > MAX_PAGES_PER_READ {
        bail!(
            "pages {pages:?} is {} pages; read at most {MAX_PAGES_PER_READ} at a time",
            range.len()
        );
    }
    Ok(range)
}

/// The PDF's page count by `pdfinfo`, or `None` without poppler.
pub async fn page_count(bytes: &[u8]) -> Result<Option<u32>> {
    let dir = tempfile::tempdir()?;
    let pdf = write_copy(dir.path(), bytes)?;
    let out = match run("pdfinfo", &[pdf.as_os_str()]).await {
        Ok(out) => out,
        Err(e) if is_missing(&e) => return Ok(None),
        Err(e) => return Err(e),
    };
    let text = String::from_utf8_lossy(&out);
    text.lines()
        .find_map(|line| line.strip_prefix("Pages:"))
        .and_then(|n| n.trim().parse().ok())
        .map(Some)
        .ok_or_else(|| anyhow!("pdfinfo gave no page count"))
}

/// Pages `range` as images, drawn by `pdftoppm` at most [`MAX_EDGE`]
/// pixels on the long edge, then fitted to the model's limits as Read's
/// images are. One per page, in order.
pub async fn page_images(bytes: &[u8], range: PageRange) -> Result<Vec<PreparedImage>> {
    let dir = tempfile::tempdir()?;
    let pdf = write_copy(dir.path(), bytes)?;
    let prefix = dir.path().join("page");
    let (first, last, edge) = (
        range.first.to_string(),
        range.last.to_string(),
        MAX_EDGE.to_string(),
    );
    run(
        "pdftoppm",
        &[
            "-png".as_ref(),
            "-f".as_ref(),
            first.as_ref(),
            "-l".as_ref(),
            last.as_ref(),
            "-scale-to".as_ref(),
            edge.as_ref(),
            pdf.as_os_str(),
            prefix.as_os_str(),
        ],
    )
    .await?;
    // pdftoppm names pages `page-<n>.png`, zero-padded to the page count's
    // width; sorting by the number keeps them in order either way.
    let mut pages: Vec<(u32, std::path::PathBuf)> = std::fs::read_dir(dir.path())?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter_map(|path| {
            let name = path.file_name()?.to_str()?;
            let n = name
                .strip_prefix("page-")?
                .strip_suffix(".png")?
                .parse()
                .ok()?;
            Some((n, path))
        })
        .collect();
    pages.sort_by_key(|(n, _)| *n);
    pages
        .into_iter()
        .map(|(_, path)| super::media::prepare_image(std::fs::read(path)?, "image/png"))
        .collect()
}

/// The text of pages `range` by `pdftotext`, each page under a
/// `--- page N ---` line, so the model can cite pages.
pub async fn page_text(bytes: &[u8], range: PageRange) -> Result<String> {
    let dir = tempfile::tempdir()?;
    let pdf = write_copy(dir.path(), bytes)?;
    let (first, last) = (range.first.to_string(), range.last.to_string());
    let out = run(
        "pdftotext",
        &[
            "-layout".as_ref(),
            "-f".as_ref(),
            first.as_ref(),
            "-l".as_ref(),
            last.as_ref(),
            pdf.as_os_str(),
            "-".as_ref(),
        ],
    )
    .await?;
    let text = String::from_utf8_lossy(&out);
    // pdftotext ends each page with a form feed.
    let mut pages = String::new();
    for (i, page) in text.split('\u{c}').enumerate() {
        let number = range.first + i as u32;
        if number > range.last {
            break;
        }
        pages.push_str(&format!("--- page {number} ---\n"));
        pages.push_str(page.trim_end());
        pages.push_str("\n\n");
    }
    Ok(pages.trim_end().to_string())
}

/// What Read returns for the PDF at `path`: its text for the model, and
/// the PDF or its pages as media.
///
/// - Without `pages`, a PDF of at most [`MAX_WHOLE_PAGES`] pages: whole,
///   for a model that `takes_pdfs`; otherwise as for its whole range.
///   Longer is an error that gives the page count and asks for a range.
/// - With `pages` (or for a model that doesn't take PDFs): page images for
///   a model that `takes_images`, else the pages' text, cut at
///   `max_bytes`.
pub async fn read(
    path: &str,
    bytes: Vec<u8>,
    pages: Option<&str>,
    takes_pdfs: bool,
    takes_images: bool,
    max_bytes: usize,
) -> Result<(String, Vec<crate::providers::ImageData>)> {
    let requested = pages.map(parse_pages).transpose()?;
    let count = page_count(&bytes)
        .await
        .map_err(|e| anyhow!("{path} looks like a PDF but can't be read as one: {e}"))?;
    let of_count = |n: Option<u32>| match n {
        Some(1) => ", 1 page".to_string(),
        Some(n) => format!(", {n} pages"),
        None => String::new(),
    };
    let range = match (requested, count) {
        (Some(range), Some(n)) if range.first > n => {
            bail!(
                "{path} has {n} page{}; {range} is past its end.",
                if n == 1 { "" } else { "s" }
            )
        }
        (Some(range), Some(n)) => PageRange {
            first: range.first,
            last: range.last.min(n),
        },
        (Some(range), None) => range,
        (None, Some(n)) if n > MAX_WHOLE_PAGES => bail!(
            "{path} is a PDF of {n} pages, more than Read returns whole \
             ({MAX_WHOLE_PAGES}). Read it in ranges with pages, such as \"1-10\", \
             at most {MAX_PAGES_PER_READ} pages at a time."
        ),
        (None, _) if takes_pdfs && bytes.len() <= MAX_WHOLE_PDF_BYTES => {
            let pdf = crate::providers::ImageData {
                media_type: crate::providers::PDF_MEDIA_TYPE.to_string(),
                base64: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes),
            };
            return Ok((format!("{path}: a PDF{}.", of_count(count)), vec![pdf]));
        }
        (None, Some(n)) => PageRange { first: 1, last: n },
        (None, None) if takes_pdfs => bail!(
            "{path} is a PDF too large to send whole ({} MB). Read it in ranges with \
             pages, such as \"1-10\"; that needs poppler: `brew install poppler` on \
             macOS, `apt-get install poppler-utils` on Debian and Ubuntu.",
            bytes.len() / (1024 * 1024)
        ),
        (None, None) => bail!(
            "{path} is a PDF, and reading one for this model needs poppler, which is \
             not installed: `brew install poppler` on macOS, `apt-get install \
             poppler-utils` on Debian and Ubuntu."
        ),
    };
    if takes_images || takes_pdfs {
        let images = page_images(&bytes, range).await?;
        let sizes: Vec<String> = images.iter().map(|i| i.description.clone()).collect();
        return Ok((
            format!(
                "{path}: {range}{}, as images ({}).",
                of_count(count).replacen(',', " of", 1),
                sizes.join("; ")
            ),
            images.into_iter().map(|i| i.image).collect(),
        ));
    }
    let text = page_text(&bytes, range).await?;
    if text
        .lines()
        .all(|l| l.starts_with("--- page ") || l.trim().is_empty())
    {
        bail!(
            "{path}: {range} has no text to extract (a scan, perhaps), and this model \
             doesn't take images."
        );
    }
    let header = format!(
        "{path}: {range}{}, as text.\n\n",
        of_count(count).replacen(',', " of", 1)
    );
    if header.len() + text.len() > max_bytes {
        bail!(
            "{path}: the text of {range} is {} KB, more than one read returns ({} KB). \
             Read fewer pages at a time.",
            text.len() / 1024,
            max_bytes / 1024
        );
    }
    Ok((header + &text, Vec::new()))
}

/// Whether `e` says a poppler tool isn't installed.
pub fn is_missing(e: &anyhow::Error) -> bool {
    e.to_string().contains("is not installed")
}

/// A copy of `bytes` as `doc.pdf` in `dir`.
fn write_copy(dir: &Path, bytes: &[u8]) -> Result<std::path::PathBuf> {
    let path = dir.join("doc.pdf");
    std::fs::write(&path, bytes)?;
    Ok(path)
}

/// Run poppler's `tool` with `args`; its stdout, or an error with its
/// stderr. A tool that isn't on `PATH` says how to install poppler, as
/// Claude Code's "pdftoppm is not installed" does.
async fn run(tool: &str, args: &[&std::ffi::OsStr]) -> Result<Vec<u8>> {
    let child = tokio::process::Command::new(tool)
        .args(args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let out = match tokio::time::timeout(TOOL_TIMEOUT, child).await {
        Err(_) => bail!("{tool} took more than {} s", TOOL_TIMEOUT.as_secs()),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => bail!(
            "{tool} is not installed. Install poppler for it: `brew install poppler` on \
             macOS, `apt-get install poppler-utils` on Debian and Ubuntu."
        ),
        Ok(out) => out.with_context(|| format!("running {tool}"))?,
    };
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("{tool} failed: {}", stderr.trim());
    }
    Ok(out.stdout)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A valid PDF of `pages` pages, page N showing "Page N" (and the text
    /// of `extra` on its last page), built by hand so tests need no PDF
    /// library.
    pub(crate) fn sample_pdf(pages: u32, extra: &str) -> Vec<u8> {
        let mut objects: Vec<String> = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".into(),
            String::new(), // the page tree, filled in below
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
        ];
        let mut kids = Vec::new();
        for n in 1..=pages {
            let text = if n == pages && !extra.is_empty() {
                format!("Page {n} {extra}")
            } else {
                format!("Page {n}")
            };
            let stream = format!("BT /F1 24 Tf 72 720 Td ({text}) Tj ET");
            objects.push(format!(
                "<< /Length {} >>\nstream\n{stream}\nendstream",
                stream.len()
            ));
            let content = objects.len();
            objects.push(format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
                 /Resources << /Font << /F1 3 0 R >> >> /Contents {content} 0 R >>"
            ));
            kids.push(format!("{} 0 R", objects.len()));
        }
        objects[1] = format!(
            "<< /Type /Pages /Kids [{}] /Count {pages} >>",
            kids.join(" ")
        );
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend(format!("{} 0 obj\n{object}\nendobj\n", i + 1).bytes());
        }
        let xref = pdf.len();
        pdf.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).bytes());
        for offset in offsets {
            pdf.extend(format!("{offset:010} 00000 n \n").bytes());
        }
        pdf.extend(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .bytes(),
        );
        pdf
    }

    /// Whether poppler is installed; tests that need it skip without it, as
    /// CI's macOS runners lack it.
    pub(crate) fn has_poppler() -> bool {
        std::process::Command::new("pdftoppm")
            .arg("-v")
            .output()
            .is_ok()
    }

    #[test]
    fn pages_takes_a_page_or_a_range_of_at_most_twenty() {
        assert_eq!(parse_pages("3").unwrap(), PageRange { first: 3, last: 3 });
        assert_eq!(
            parse_pages(" 1-5 ").unwrap(),
            PageRange { first: 1, last: 5 }
        );
        assert_eq!(parse_pages("1-20").unwrap().len(), 20);
        for bad in ["0", "", "3-", "-3", "a", "5-2", "1-21"] {
            assert!(parse_pages(bad).is_err(), "{bad:?}");
        }
        assert_eq!(parse_pages("2-4").unwrap().to_string(), "pages 2-4");
        assert_eq!(parse_pages("7").unwrap().to_string(), "page 7");
    }

    #[tokio::test]
    async fn poppler_counts_draws_and_reads_pages() {
        if !has_poppler() {
            eprintln!("poppler not installed; skipping");
            return;
        }
        let pdf = sample_pdf(3, "the answer is 42");
        assert_eq!(page_count(&pdf).await.unwrap(), Some(3));

        let images = page_images(&pdf, parse_pages("2-3").unwrap())
            .await
            .unwrap();
        assert_eq!(images.len(), 2);
        assert_eq!(images[0].image.media_type, "image/png");
        // Letter size, drawn at most MAX_EDGE on the long edge.
        assert!(
            images[0].description.contains(&format!("×{MAX_EDGE}")),
            "{}",
            images[0].description
        );

        let text = page_text(&pdf, parse_pages("2-3").unwrap()).await.unwrap();
        assert!(text.starts_with("--- page 2 ---\n"), "{text}");
        assert!(text.contains("--- page 3 ---\n"), "{text}");
        assert!(text.contains("the answer is 42"), "{text}");
        assert!(!text.contains("Page 1"), "{text}");
    }

    #[tokio::test]
    async fn a_file_that_isnt_a_pdf_is_an_error() {
        if !has_poppler() {
            return;
        }
        assert!(page_count(b"%PDF-1.4 not really").await.is_err());
    }
}
