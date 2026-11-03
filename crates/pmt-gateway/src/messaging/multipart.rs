//! `multipart/form-data` bodies for adapters that upload files (the Bot
//! API, Discord attachments): text fields and files, one random boundary.

use std::fmt::Write as _;

/// A form being built.
#[derive(Debug)]
pub struct Form {
    boundary: String,
    body: Vec<u8>,
}

impl Default for Form {
    fn default() -> Self {
        Self::new()
    }
}

impl Form {
    /// An empty form.
    #[must_use]
    pub fn new() -> Self {
        Self {
            boundary: format!("prometheus-{}", crate::tickets::random_hex(12)),
            body: Vec::new(),
        }
    }

    fn head(&mut self, disposition: &str, content_type: Option<&str>) {
        let mut head = format!(
            "--{}\r\nContent-Disposition: form-data; {disposition}\r\n",
            self.boundary
        );
        if let Some(kind) = content_type {
            let _ = write!(head, "Content-Type: {kind}\r\n");
        }
        head.push_str("\r\n");
        self.body.extend_from_slice(head.as_bytes());
    }

    /// Adds a text field.
    pub fn text(&mut self, name: &str, value: &str) {
        self.head(&format!("name=\"{}\"", escape(name)), None);
        self.body.extend_from_slice(value.as_bytes());
        self.body.extend_from_slice(b"\r\n");
    }

    /// Adds a file.
    pub fn file(&mut self, name: &str, file_name: &str, bytes: &[u8]) {
        let disposition = format!(
            "name=\"{}\"; filename=\"{}\"",
            escape(name),
            escape(file_name)
        );
        self.head(&disposition, Some("application/octet-stream"));
        self.body.extend_from_slice(bytes);
        self.body.extend_from_slice(b"\r\n");
    }

    /// The `Content-Type` header value.
    #[must_use]
    pub fn content_type(&self) -> String {
        format!("multipart/form-data; boundary={}", self.boundary)
    }

    /// The finished body.
    #[must_use]
    pub fn finish(mut self) -> Vec<u8> {
        self.body
            .extend_from_slice(format!("--{}--\r\n", self.boundary).as_bytes());
        self.body
    }
}

/// Quotes and line breaks cannot end a header parameter early.
fn escape(value: &str) -> String {
    value.replace(['"', '\r', '\n'], "_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_form_frames_each_part_with_its_boundary() {
        let mut form = Form::new();
        form.text("chat_id", "42");
        form.file("photo", "a\"b.png", b"PNG");
        let kind = form.content_type();
        let boundary = kind.split("boundary=").nth(1).unwrap().to_owned();
        let body = String::from_utf8(form.finish()).unwrap();
        assert!(body.starts_with(&format!("--{boundary}\r\n")));
        assert!(body.contains("name=\"chat_id\"\r\n\r\n42\r\n"));
        assert!(body.contains("filename=\"a_b.png\""));
        assert!(body.ends_with(&format!("--{boundary}--\r\n")));
    }
}
