//! What Read returns for a Jupyter notebook (`.ipynb`), as Claude Code's Read
//! does: every cell with its source and outputs, rather than the notebook's
//! JSON, in which the source hides inside escaped strings and outputs carry
//! base64 images. An output image is sent as an image to a model that takes
//! images and becomes a one-line note for one that doesn't.
//!
//! The limits are Claude Code's: cells that come to more than
//! [`MAX_RENDERED_BYTES`](crate::tools::notebook::MAX_RENDERED_BYTES) (or more than one tool result holds) are an error
//! that says how to read part of the notebook, and a file over
//! [`MAX_NOTEBOOK_FILE_BYTES`](crate::tools::notebook::MAX_NOTEBOOK_FILE_BYTES) is refused before it is read. A read with
//! `start_line` or `num_lines` returns the JSON's numbered lines instead,
//! which is what an `Edit` of the file matches against.

use base64::Engine;
use serde_json::Value;

use crate::providers::ImageData;

/// The most text a notebook's cells may come to in one read, as Claude
/// Code's 256 KB.
pub const MAX_RENDERED_BYTES: usize = 256 * 1024;

/// The largest notebook file Read takes, as Claude Code's 100 MB.
pub const MAX_NOTEBOOK_FILE_BYTES: u64 = 100 * 1024 * 1024;

/// A notebook as Read returns it.
#[derive(Debug, Default)]
pub struct Rendered {
    /// The cells and their outputs, as text.
    pub text: String,
    /// Output images, in order, for a model that takes images.
    pub images: Vec<ImageData>,
}

/// Whether `path` names a notebook, by its extension, and `bytes` parse as
/// one: a JSON object with a `cells` list. A file that only has the
/// extension is read as text.
pub fn parse(path: &str, bytes: &[u8]) -> Option<Value> {
    if !path.to_ascii_lowercase().ends_with(".ipynb") {
        return None;
    }
    let notebook: Value = serde_json::from_slice(bytes).ok()?;
    notebook.get("cells")?.as_array()?;
    Some(notebook)
}

/// `notebook`'s cells and outputs as text, with output images given back as
/// images where `takes_images`, else noted in the text. Images are scaled
/// to the model's limits as Read's are.
pub fn render(notebook: &Value, takes_images: bool) -> Rendered {
    let mut out = Rendered::default();
    let language = notebook
        .pointer("/metadata/language_info/name")
        .or_else(|| notebook.pointer("/metadata/kernelspec/language"))
        .and_then(Value::as_str);
    let cells = notebook["cells"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    out.text.push_str(&format!(
        "A Jupyter notebook: {} cell{}{}.\n",
        cells.len(),
        if cells.len() == 1 { "" } else { "s" },
        language.map(|l| format!(", {l}")).unwrap_or_default()
    ));
    for (index, cell) in cells.iter().enumerate() {
        let kind = cell["cell_type"].as_str().unwrap_or("code");
        let mut attrs = format!("index=\"{}\" type=\"{kind}\"", index + 1);
        if let Some(id) = cell["id"].as_str() {
            attrs.push_str(&format!(" id=\"{id}\""));
        }
        if let Some(count) = cell["execution_count"].as_u64() {
            attrs.push_str(&format!(" execution_count=\"{count}\""));
        }
        out.text.push_str(&format!("\n<cell {attrs}>\n"));
        push_line(&mut out.text, &joined(&cell["source"]));
        for output in cell["outputs"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
            render_output(output, takes_images, &mut out);
        }
        out.text.push_str("</cell>\n");
    }
    out
}

/// One output of a code cell: printed text, a result or display (its text,
/// or its image), or an error with its traceback.
fn render_output(output: &Value, takes_images: bool, out: &mut Rendered) {
    match output["output_type"].as_str().unwrap_or_default() {
        "stream" => {
            let name = output["name"].as_str().unwrap_or("stdout");
            out.text.push_str(&format!("<output type=\"{name}\">\n"));
            push_line(&mut out.text, &strip_ansi(&joined(&output["text"])));
            out.text.push_str("</output>\n");
        }
        "error" => {
            let name = output["ename"].as_str().unwrap_or("Error");
            let value = output["evalue"].as_str().unwrap_or_default();
            out.text.push_str("<output type=\"error\">\n");
            let traceback: Vec<String> = output["traceback"]
                .as_array()
                .map(|lines| {
                    lines
                        .iter()
                        .filter_map(Value::as_str)
                        .map(strip_ansi)
                        .collect()
                })
                .unwrap_or_default();
            if traceback.is_empty() {
                push_line(&mut out.text, &format!("{name}: {value}"));
            } else {
                push_line(&mut out.text, &traceback.join("\n"));
            }
            out.text.push_str("</output>\n");
        }
        kind @ ("execute_result" | "display_data") => {
            let data = &output["data"];
            for media_type in ["image/png", "image/jpeg", "image/gif", "image/webp"] {
                if let Some(encoded) = data.get(media_type) {
                    render_image(media_type, &joined(encoded), takes_images, out);
                }
            }
            let text = ["text/plain", "text/markdown", "text/latex"]
                .iter()
                .find_map(|t| data.get(*t))
                .map(joined);
            match text {
                Some(text) => {
                    let kind = if kind == "execute_result" {
                        "result"
                    } else {
                        "display"
                    };
                    out.text.push_str(&format!("<output type=\"{kind}\">\n"));
                    push_line(&mut out.text, &strip_ansi(&text));
                    out.text.push_str("</output>\n");
                }
                None if !has_image(data) => {
                    let types: Vec<&str> = data
                        .as_object()
                        .map(|m| m.keys().map(String::as_str).collect())
                        .unwrap_or_default();
                    out.text.push_str(&format!(
                        "<output type=\"{}\" not_shown=\"{}\"/>\n",
                        if kind == "execute_result" {
                            "result"
                        } else {
                            "display"
                        },
                        types.join(", ")
                    ));
                }
                None => {}
            }
        }
        _ => {}
    }
}

fn has_image(data: &Value) -> bool {
    data.as_object().is_some_and(|m| {
        m.keys()
            .any(|k| k.starts_with("image/") && k != "image/svg+xml")
    })
}

/// An output image: sent as one where the model takes images, else a note.
fn render_image(media_type: &str, encoded: &str, takes_images: bool, out: &mut Rendered) {
    if !takes_images {
        out.text
            .push_str("<output type=\"image\">an image; this model doesn't take images</output>\n");
        return;
    }
    let compact: String = encoded.chars().filter(|c| !c.is_whitespace()).collect();
    let prepared = base64::engine::general_purpose::STANDARD
        .decode(compact.as_bytes())
        .map_err(anyhow::Error::from)
        .and_then(|bytes| super::media::prepare_image(bytes, media_type));
    match prepared {
        Ok(prepared) => {
            out.images.push(prepared.image);
            out.text.push_str(&format!(
                "<output type=\"image\">image {} of this read, {}</output>\n",
                out.images.len(),
                prepared.description
            ));
        }
        Err(e) => out.text.push_str(&format!(
            "<output type=\"image\">an image that can't be read: {e}</output>\n"
        )),
    }
}

/// A notebook string field, which is either a string or a list of lines.
fn joined(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts.iter().filter_map(Value::as_str).collect(),
        _ => String::new(),
    }
}

/// `text`, ending with exactly one newline (none for empty text).
fn push_line(buf: &mut String, text: &str) {
    let text = text.trim_end_matches('\n');
    if !text.is_empty() {
        buf.push_str(text);
        buf.push('\n');
    }
}

/// `text` without terminal colour codes, which tracebacks carry.
fn strip_ansi(text: &str) -> String {
    static ANSI: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"\x1b\[[0-9;?]*[A-Za-z]").unwrap());
    ANSI.replace_all(text, "").into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A 1×1 PNG.
    const PIXEL: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";

    fn notebook() -> Value {
        json!({
            "nbformat": 4,
            "metadata": {"language_info": {"name": "python"}},
            "cells": [
                {"cell_type": "markdown", "id": "intro", "source": ["# Title\n", "Some text"]},
                {"cell_type": "code", "id": "c1", "execution_count": 1,
                 "source": "print('hi')\n1 + 1",
                 "outputs": [
                     {"output_type": "stream", "name": "stdout", "text": ["hi\n"]},
                     {"output_type": "execute_result", "data": {"text/plain": ["2"]}}
                 ]},
                {"cell_type": "code", "id": "c2", "execution_count": 2, "source": "plot()",
                 "outputs": [
                     {"output_type": "display_data",
                      "data": {"image/png": PIXEL, "text/plain": ["<Figure size 640x480>"]}}
                 ]},
                {"cell_type": "code", "id": "c3", "execution_count": 3, "source": "1/0",
                 "outputs": [
                     {"output_type": "error", "ename": "ZeroDivisionError",
                      "evalue": "division by zero",
                      "traceback": ["\x1b[0;31mZeroDivisionError\x1b[0m: division by zero"]}
                 ]},
                {"cell_type": "code", "id": "c4", "source": "df",
                 "outputs": [
                     {"output_type": "execute_result",
                      "data": {"text/html": ["<table></table>"], "application/json": {}}}
                 ]}
            ]
        })
    }

    #[test]
    fn cells_come_back_with_their_outputs() {
        let r = render(&notebook(), true);
        assert!(
            r.text.starts_with("A Jupyter notebook: 5 cells, python.\n"),
            "{}",
            r.text
        );
        assert!(
            r.text.contains(
                "<cell index=\"1\" type=\"markdown\" id=\"intro\">\n# Title\nSome text\n</cell>"
            ),
            "{}",
            r.text
        );
        assert!(
            r.text.contains(
                "<cell index=\"2\" type=\"code\" id=\"c1\" execution_count=\"1\">\nprint('hi')\n1 + 1\n\
                 <output type=\"stdout\">\nhi\n</output>\n<output type=\"result\">\n2\n</output>\n</cell>"
            ),
            "{}",
            r.text
        );
    }

    #[test]
    fn an_error_shows_its_traceback_without_colour_codes() {
        let r = render(&notebook(), true);
        assert!(
            r.text.contains(
                "<output type=\"error\">\nZeroDivisionError: division by zero\n</output>"
            ),
            "{}",
            r.text
        );
        assert!(!r.text.contains('\u{1b}'));
    }

    #[test]
    fn an_output_image_is_sent_where_the_model_takes_images() {
        let r = render(&notebook(), true);
        assert_eq!(r.images.len(), 1);
        assert_eq!(r.images[0].media_type, "image/png");
        assert!(
            r.text
                .contains("<output type=\"image\">image 1 of this read, 1×1</output>"),
            "{}",
            r.text
        );
        // The figure's text form comes along too.
        assert!(r.text.contains("<Figure size 640x480>"));
    }

    #[test]
    fn an_output_image_is_a_note_for_a_text_only_model() {
        let r = render(&notebook(), false);
        assert!(r.images.is_empty());
        assert!(
            r.text.contains("an image; this model doesn't take images"),
            "{}",
            r.text
        );
    }

    #[test]
    fn an_output_with_no_text_or_image_names_its_types() {
        let r = render(&notebook(), true);
        assert!(
            r.text
                .contains("<output type=\"result\" not_shown=\"text/html, application/json\"/>")
                || r.text.contains(
                    "<output type=\"result\" not_shown=\"application/json, text/html\"/>"
                ),
            "{}",
            r.text
        );
    }

    #[test]
    fn only_an_ipynb_that_parses_with_cells_is_a_notebook() {
        let bytes = serde_json::to_vec(&notebook()).unwrap();
        assert!(parse("a.ipynb", &bytes).is_some());
        assert!(parse("A.IPYNB", &bytes).is_some());
        assert!(parse("a.json", &bytes).is_none());
        assert!(parse("a.ipynb", b"not json").is_none());
        assert!(parse("a.ipynb", br#"{"metadata": {}}"#).is_none());
    }
}
