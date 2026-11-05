//! The `doc-converter` runtime (spec: execution-environments, `read_file`
//! document extraction): the converter protocol and its confinement with
//! a stand-in converter, and the pinned runtime itself installed from the
//! runtime index (CI sets `PMT_TEST_DOC_CONVERTER=require`; elsewhere that
//! test skips, since it downloads the real artifact).

use std::path::Path;
use std::sync::Arc;

use pmt_tools::browser::RuntimePolicy;
use pmt_tools::files::documents::{self, Converted, DocConverter};
use prometheus::chat::session::documents::{Confinement, RuntimeConverter};

fn confinement(home: &Path) -> Confinement {
    Confinement {
        work: home.join("cache/documents"),
        user_home: home.join("user"),
        prometheus_home: home.to_path_buf(),
        sandbox: pmt_config::schema::SandboxConfig::default(),
    }
}

async fn convert(converter: RuntimeConverter, path: &Path, ext: &'static str) -> Converted {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || converter.convert(&path, ext))
        .await
        .unwrap()
        .unwrap()
}

#[cfg(unix)] // the stand-in converter is a shell script
#[tokio::test(flavor = "multi_thread")]
async fn a_converter_on_path_answers_with_pages_from_inside_the_sandbox() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let docs = dir.path().join("docs");
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    let pdf = docs.join("scan.pdf");
    std::fs::write(&pdf, b"%PDF-1.7").unwrap();
    // The stand-in reads its private copy, then tries to reach the
    // original document's directory: the sandbox must refuse both.
    let script = bin.join("xberg");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\n[ \"$1\" = extract ] || exit 2\n[ -r \"$2\" ] || exit 3\n\
             case \"$2\" in {docs}/*) exit 4;; esac\n\
             printf x > {docs}/escaped 2>/dev/null\n\
             cat {docs}/scan.pdf > \"$TMPDIR/leak\" 2>/dev/null\n\
             [ -s \"$TMPDIR/leak\" ] && exit 5\n\
             printf '{{\"result\": {{\"content\": \"x\", \"pages\": [{{\"page_number\": 1, \"content\": \"Intro\"}}, {{\"page_number\": 2, \"content\": \"\"}}, {{\"page_number\": 3, \"content\": \"End\"}}]}}}}'\n",
            docs = docs.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let converter = RuntimeConverter::new(
        RuntimePolicy {
            runtimes: Arc::new(pmt_tools::browser::launch::NoRuntimes),
            allow_lazy_install: false,
            host_candidates: vec![script],
        },
        tokio::runtime::Handle::current(),
        confinement(&dir.path().join("home")),
    );
    let pages = convert(converter, &pdf, "pdf").await;
    assert_eq!(
        pages,
        Converted::Pages(vec!["Intro".into(), String::new(), "End".into()])
    );
    assert!(
        !docs.join("escaped").exists(),
        "the converter wrote outside its temp dir"
    );
    let work = dir.path().join("home/cache/documents");
    assert_eq!(
        std::fs::read_dir(work).unwrap().count(),
        0,
        "the private copy is removed"
    );
}

/// The pinned `doc-converter` runtime, installed from the runtime index
/// (sha256-verified), reads a PDF with a page without text and an RTF.
#[tokio::test(flavor = "multi_thread")]
async fn the_pinned_runtime_converts_a_pdf_and_an_rtf() {
    if std::env::var("PMT_TEST_DOC_CONVERTER").as_deref() != Ok("require") {
        eprintln!(
            "skipped: set PMT_TEST_DOC_CONVERTER=require to download the pinned doc-converter"
        );
        return;
    }
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let policies = prometheus::runtimes_cmd::browser::for_home(
        &home,
        &pmt_config::schema::NetworkConfig::default(),
        None,
        None,
    )
    .unwrap();
    let converter = || {
        RuntimeConverter::new(
            policies.documents.clone(),
            tokio::runtime::Handle::current(),
            confinement(&home),
        )
    };

    let pdf = dir.path().join("scan.pdf");
    std::fs::write(&pdf, pdf_of(&["Introduction alpha", "", "Closing omega"])).unwrap();
    let Converted::Pages(pages) = convert(converter(), &pdf, "pdf").await else {
        panic!("a PDF converts to pages");
    };
    assert_eq!(pages.len(), 3, "{pages:?}");
    assert!(pages[0].contains("Introduction alpha"), "{pages:?}");
    assert!(pages[1].trim().is_empty(), "{pages:?}");
    assert!(pages[2].contains("Closing omega"), "{pages:?}");
    let warning = documents::coverage_warning(&pages).unwrap();
    assert!(
        warning.contains("1 of 3 pages")
            && warning.contains("page 2 (1 page), after: \"Introduction alpha\""),
        "{warning}"
    );

    let rtf = dir.path().join("note.rtf");
    std::fs::write(
        &rtf,
        br"{\rtf1\ansi\deff0 {\fonttbl {\f0 Times;}}\f0 Rich text body\par}",
    )
    .unwrap();
    let Converted::Markdown(text) = convert(converter(), &rtf, "rtf").await else {
        panic!("an RTF converts to Markdown");
    };
    assert!(text.contains("Rich text body"), "{text}");
}

/// A PDF with one Helvetica text line per page (an empty string leaves
/// that page without a text layer).
fn pdf_of(texts: &[&str]) -> Vec<u8> {
    let count = texts.len();
    let font = 3 + 2 * count;
    let kids: Vec<String> = (0..count).map(|i| format!("{} 0 R", 3 + 2 * i)).collect();
    let mut objects = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        format!(
            "<< /Type /Pages /Kids [{}] /Count {count} >>",
            kids.join(" ")
        ),
    ];
    for (i, text) in texts.iter().enumerate() {
        objects.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents {} 0 R /Resources << /Font << /F1 {font} 0 R >> >> >>",
            4 + 2 * i
        ));
        let stream = if text.is_empty() {
            String::new()
        } else {
            format!("BT /F1 24 Tf 72 700 Td ({text}) Tj ET")
        };
        objects.push(format!(
            "<< /Length {} >>\nstream\n{stream}\nendstream",
            stream.len()
        ));
    }
    objects.push("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_owned());
    let mut pdf = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", i + 1).as_bytes());
    }
    let xref = pdf.len();
    pdf.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for offset in offsets {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    pdf
}
