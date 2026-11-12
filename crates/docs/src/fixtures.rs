//! Tiny generated documents for tests (here and in crates that use byob-docs): a PDF
//! with text and a bar "chart", a .docx and a .pptx. Not for production use.

use std::io::{Cursor, Write};

/// A small valid PDF: one page per entry of `pages`, each a list of (x, y, size, text)
/// in Helvetica (a standard font, not embedded), plus `bars` filled rectangles on the
/// last page (a "chart").
pub fn make_pdf(pages: &[&[(u32, u32, u32, &str)]], bars: usize) -> Vec<u8> {
    let mut objects: Vec<String> = Vec::new();
    let n = pages.len();
    // 1: catalog, 2: pages, 3: font, then per page: page object and content stream.
    let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", 4 + i * 2)).collect();
    objects.push("<< /Type /Catalog /Pages 2 0 R >>".into());
    objects.push(format!("<< /Type /Pages /Kids [{}] /Count {n} >>", kids.join(" ")));
    objects.push("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".into());
    for (i, lines) in pages.iter().enumerate() {
        let mut content = String::new();
        for (x, y, size, text) in lines.iter() {
            content.push_str(&format!("BT /F1 {size} Tf {x} {y} Td ({text}) Tj ET\n"));
        }
        if i + 1 == n {
            for b in 0..bars {
                content.push_str(&format!("0.2 0.4 0.8 rg {} 100 30 {} re f\n", 100 + b * 50, 40 + b * 30));
            }
        }
        objects.push(format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 3 0 R >> >> /Contents {} 0 R >>", 5 + i * 2));
        objects.push(format!("<< /Length {} >>\nstream\n{content}endstream", content.len()));
    }
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, o) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes());
    for off in offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objects.len() + 1).as_bytes());
    out
}

/// A zip of text parts (an Office file's skeleton).
pub fn make_zip(parts: &[(&str, &str)]) -> Vec<u8> {
    let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, body) in parts {
        if w.start_file(*name, zip::write::SimpleFileOptions::default()).is_ok() {
            let _ = w.write_all(body.as_bytes());
        }
    }
    w.finish().map(Cursor::into_inner).unwrap_or_default()
}

const W: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#;

pub fn sample_docx() -> Vec<u8> {
    let body = format!(
        r#"<?xml version="1.0"?><w:document {W}><w:body>
<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>Annual report</w:t></w:r></w:p>
<w:p><w:r><w:t xml:space="preserve">Revenue rose </w:t></w:r><w:r><w:rPr><w:b/></w:rPr><w:t>12 &amp; more</w:t></w:r><w:r><w:t>%.</w:t></w:r></w:p>
<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>First point</w:t></w:r></w:p>
<w:p><w:pPr><w:numPr><w:ilvl w:val="1"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>Sub point</w:t></w:r></w:p>
<w:tbl><w:tr><w:tc><w:p><w:r><w:t>Region</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>Sales</w:t></w:r></w:p></w:tc></w:tr>
<w:tr><w:tc><w:p><w:r><w:t>North</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>40</w:t></w:r></w:p></w:tc></w:tr></w:tbl>
<w:p><w:pPr><w:pStyle w:val="Heading2"/></w:pPr><w:r><w:t>Outlook</w:t></w:r></w:p>
<w:p><w:r><w:t>Line one</w:t></w:r><w:r><w:br/><w:t>line two</w:t></w:r></w:p>
</w:body></w:document>"#
    );
    make_zip(&[("[Content_Types].xml", "<Types/>"), ("word/document.xml", &body)])
}

pub fn sample_pptx() -> Vec<u8> {
    let a = r#"xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships""#;
    let slide = |title: &str, bullets: &[&str]| {
        let items: String = bullets.iter().map(|b| format!("<a:p><a:pPr lvl=\"0\"/><a:r><a:t>{b}</a:t></a:r></a:p>")).collect();
        format!(
            r#"<p:sld {a}><p:cSld><p:spTree>
<p:sp><p:nvSpPr><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>{title}</a:t></a:r></a:p></p:txBody></p:sp>
<p:sp><p:nvSpPr><p:nvPr><p:ph idx="1"/></p:nvPr></p:nvSpPr><p:txBody>{items}</p:txBody></p:sp>
</p:spTree></p:cSld></p:sld>"#
        )
    };
    // Presentation order is slide2.xml then slide1.xml.
    let pres = format!(r#"<p:presentation {a}><p:sldIdLst><p:sldId id="256" r:id="rId3"/><p:sldId id="257" r:id="rId2"/></p:sldIdLst></p:presentation>"#);
    let rels = r#"<Relationships><Relationship Id="rId2" Type="slide" Target="slides/slide1.xml"/><Relationship Id="rId3" Type="slide" Target="slides/slide2.xml"/></Relationships>"#;
    make_zip(&[
        ("ppt/presentation.xml", &pres),
        ("ppt/_rels/presentation.xml.rels", rels),
        ("ppt/slides/slide1.xml", &slide("Second", &["Costs fell"])),
        ("ppt/slides/slide2.xml", &slide("Welcome", &["Agenda", "Results"])),
    ])
}

/// A two-page report: text on page 1, a heading and four bars on page 2.
pub fn sample_pdf() -> Vec<u8> {
    make_pdf(&[&[(72, 720, 20, "Quarterly report"), (72, 690, 11, "Sales grew by 12 percent.")], &[(72, 720, 14, "Chart: revenue by region")]], 4)
}
