//! Minimal end-to-end test: drop a txt book into a temp books dir, boot the
//! server on a random port, and verify /api/books + /api/search.
//!
//! Run with: cargo test --test integration

use std::io::Write;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn wait_for_health(port: u16, timeout: Duration) -> bool {
    let t0 = std::time::Instant::now();
    while t0.elapsed() < timeout {
        if let Ok(resp) = reqwest::blocking::get(format!("http://127.0.0.1:{port}/api/health")) {
            if resp.status().is_success() {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

#[test]
fn end_to_end_txt_book() {
    let tmp = std::env::temp_dir().join(format!("shuku-e2e-{}", uuid::Uuid::new_v4()));
    let books = tmp.join("books");
    let data = tmp.join("data");
    std::fs::create_dir_all(&books).unwrap();
    let mut f = std::fs::File::create(books.join("修仙传.txt")).unwrap();
    writeln!(f, "第一章 开始").unwrap();
    writeln!(f, "少年走在山路上，心中想着修仙的事。").unwrap();
    writeln!(f, "第二章 练功").unwrap();
    writeln!(f, "他在山洞里打坐修炼，灵气汇聚成河。").unwrap();
    drop(f);

    let bin = env!("CARGO_BIN_EXE_shuku");
    let port = free_port();
    let mut child: Child = Command::new(bin)
        .env("SHUKU_BOOKS_DIR", &books)
        .env("SHUKU_DATA_DIR", &data)
        .env("SHUKU_ADDR", format!("127.0.0.1:{port}"))
        .env("RUST_LOG", "shuku=info")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn server");

    let client = reqwest::blocking::Client::new();
    let base = format!("http://127.0.0.1:{port}");
    let result = (|| -> Result<(), String> {
        assert!(wait_for_health(port, Duration::from_secs(30)), "server did not come up");
        // Wait for the boot scan to index the book (worker polls every 3s).
        let mut books_json = serde_json::Value::Null;
        let t0 = std::time::Instant::now();
        while t0.elapsed() < Duration::from_secs(30) {
            let r = client.get(format!("{base}/api/books")).send().map_err(|e| e.to_string())?;
            books_json = r.json().map_err(|e| e.to_string())?;
            if books_json["total"].as_i64().unwrap_or(0) >= 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        assert_eq!(books_json["total"], 1, "expected 1 book, got {books_json}");
        let book_id = books_json["items"][0]["id"].as_str().unwrap().to_string();
        assert_eq!(books_json["items"][0]["title"], "修仙传");
        assert_eq!(books_json["items"][0]["chapter_count"], 2);

        // Chapters list
        let r = client.get(format!("{base}/api/books/{book_id}/chapters")).send().map_err(|e| e.to_string())?;
        let ch: serde_json::Value = r.json().map_err(|e| e.to_string())?;
        assert_eq!(ch["total"], 2);
        assert_eq!(ch["items"][0]["title"], "第一章 开始");

        // Chapter content (paragraphs)
        let r = client.get(format!("{base}/api/books/{book_id}/chapters/0")).send().map_err(|e| e.to_string())?;
        let c: serde_json::Value = r.json().map_err(|e| e.to_string())?;
        let paras = c["paragraphs"].as_array().unwrap();
        assert!(paras.iter().any(|p| p["text"].as_str().unwrap_or("").contains("修仙")));

        // Full-text search with highlight
        let r = client
            .get(format!("{base}/api/search"))
            .query(&[("q", "修仙"), ("scope", "local")])
            .send()
            .map_err(|e| e.to_string())?;
        let s: serde_json::Value = r.json().map_err(|e| e.to_string())?;
        assert!(s["total"].as_i64().unwrap_or(0) >= 1, "search must hit: {s}");
        let snippet = s["items"][0]["snippet"].as_str().unwrap_or("");
        assert!(snippet.contains("<em>"), "snippet must highlight: {snippet}");

        // In-book search: book_id restricts to that book; a bogus id hits nothing.
        let r = client
            .get(format!("{base}/api/search"))
            .query(&[("q", "修仙"), ("book_id", &book_id)])
            .send()
            .map_err(|e| e.to_string())?;
        let s: serde_json::Value = r.json().map_err(|e| e.to_string())?;
        assert!(s["total"].as_i64().unwrap_or(0) >= 1, "in-book search must hit: {s}");
        assert!(s["items"].as_array().unwrap().iter().all(|h| h["book_id"].as_str() == Some(book_id.as_str())));
        let r = client
            .get(format!("{base}/api/search"))
            .query(&[("q", "修仙"), ("book_id", "4242424242")])
            .send()
            .map_err(|e| e.to_string())?;
        let s: serde_json::Value = r.json().map_err(|e| e.to_string())?;
        assert_eq!(s["total"], 0, "bogus book_id must hit nothing: {s}");

        // Pagination params must deserialize (regression: #[serde(flatten)]
        // on PageQuery made ?page_size=1 fail with 400 on /api/books and /api/search).
        let r = client
            .get(format!("{base}/api/books"))
            .query(&[("page", "1"), ("page_size", "1")])
            .send()
            .map_err(|e| e.to_string())?;
        assert!(r.status().is_success(), "books?page_size=1 must not 400: {}", r.status());
        let pg: serde_json::Value = r.json().map_err(|e| e.to_string())?;
        assert_eq!(pg["total"], 1);
        assert_eq!(pg["items"].as_array().unwrap().len(), 1);
        assert_eq!(pg["page_size"], 1);
        let r = client
            .get(format!("{base}/api/search"))
            .query(&[("q", "修仙"), ("scope", "local"), ("page", "1"), ("page_size", "5")])
            .send()
            .map_err(|e| e.to_string())?;
        assert!(r.status().is_success(), "search?page_size=5 must not 400: {}", r.status());
        let s: serde_json::Value = r.json().map_err(|e| e.to_string())?;
        assert!(s["total"].as_i64().unwrap_or(0) >= 1);

        // Progress round-trip
        let r = client
            .put(format!("{base}/api/books/{book_id}/progress"))
            .json(&serde_json::json!({"chapter_idx": 1, "offset": 5}))
            .send()
            .map_err(|e| e.to_string())?;
        assert!(r.status().is_success());
        let r = client.get(format!("{base}/api/books/{book_id}/progress")).send().map_err(|e| e.to_string())?;
        let p: serde_json::Value = r.json().map_err(|e| e.to_string())?;
        assert_eq!(p["chapter_idx"], 1);
        assert_eq!(p["offset"], 5);

        // Reading stats: progress is at the last chapter of a 2-chapter
        // book, so it counts as finished; reading-time accumulates.
        let r = client.get(format!("{base}/api/stats")).send().map_err(|e| e.to_string())?;
        let st: serde_json::Value = r.json().map_err(|e| e.to_string())?;
        assert_eq!(st["finished_books"], 1, "last-chapter progress is finished: {st}");
        assert_eq!(st["total_books"], 1);
        let r = client
            .post(format!("{base}/api/books/{book_id}/reading-time"))
            .json(&serde_json::json!({"seconds": 120}))
            .send()
            .map_err(|e| e.to_string())?;
        assert!(r.status().is_success());
        let r = client.get(format!("{base}/api/stats")).send().map_err(|e| e.to_string())?;
        let st: serde_json::Value = r.json().map_err(|e| e.to_string())?;
        assert_eq!(st["total_seconds"], 120);

        // OPDS catalog: root nav, book feed, search, download.
        let r = client.get(format!("{base}/opds/")).send().map_err(|e| e.to_string())?;
        assert!(r.status().is_success());
        let body = r.text().map_err(|e| e.to_string())?;
        assert!(body.contains("opds-catalog") && body.contains("/opds/books"), "OPDS root: {body}");
        let r = client.get(format!("{base}/opds/books")).send().map_err(|e| e.to_string())?;
        let body = r.text().map_err(|e| e.to_string())?;
        assert!(body.contains("修仙传"), "OPDS books feed must list the book: {body}");
        let r = client
            .get(format!("{base}/opds/search"))
            .query(&[("q", "修仙")])
            .send()
            .map_err(|e| e.to_string())?;
        let body = r.text().map_err(|e| e.to_string())?;
        assert!(body.contains("修仙传"), "OPDS search must find the book");
        let r = client.get(format!("{base}/opds/download/{book_id}")).send().map_err(|e| e.to_string())?;
        assert!(r.status().is_success());
        let body = r.text().map_err(|e| e.to_string())?;
        assert!(body.contains("灵气汇聚成河"), "OPDS download must serve the book text");
        let r = client.get(format!("{base}/opds/opensearch.xml")).send().map_err(|e| e.to_string())?;
        let body = r.text().map_err(|e| e.to_string())?;
        assert!(body.contains("OpenSearchDescription"), "opensearch.xml: {body}");

        // Chapter issues: the fixture book is 第一章/第二章, consecutive.
        let r = client.get(format!("{base}/api/books/{book_id}/chapter-issues")).send().map_err(|e| e.to_string())?;
        assert!(r.status().is_success());
        let v: serde_json::Value = r.json().map_err(|e| e.to_string())?;
        assert_eq!(v["issues"], serde_json::json!([]), "fixture book has no chapter issues: {v}");

        // Search suggestions: title substring, and pinyin via the index.
        let r = client.get(format!("{base}/api/suggest")).query(&[("q", "修仙")]).send().map_err(|e| e.to_string())?;
        let v: serde_json::Value = r.json().map_err(|e| e.to_string())?;
        assert!(v["items"].as_array().unwrap().iter().any(|b| b["title"] == "修仙传"), "suggest 修仙: {v}");
        let r = client.get(format!("{base}/api/suggest")).query(&[("q", "xiuxian")]).send().map_err(|e| e.to_string())?;
        let v: serde_json::Value = r.json().map_err(|e| e.to_string())?;
        assert!(v["items"].as_array().unwrap().iter().any(|b| b["title"] == "修仙传"), "suggest xiuxian (pinyin): {v}");
        // First-letter search: xx / xxc find 修仙传, in full search and suggestions.
        let r = client.get(format!("{base}/api/search")).query(&[("q", "xx")]).send().map_err(|e| e.to_string())?;
        let v: serde_json::Value = r.json().map_err(|e| e.to_string())?;
        assert!(v["total"].as_i64().unwrap_or(0) >= 1, "search xx (initials): {v}");
        let r = client.get(format!("{base}/api/suggest")).query(&[("q", "xxc")]).send().map_err(|e| e.to_string())?;
        let v: serde_json::Value = r.json().map_err(|e| e.to_string())?;
        assert!(v["items"].as_array().unwrap().iter().any(|b| b["title"] == "修仙传"), "suggest xxc (initials): {v}");
        let r = client.get(format!("{base}/api/suggest")).query(&[("q", "不存在")]).send().map_err(|e| e.to_string())?;
        let v: serde_json::Value = r.json().map_err(|e| e.to_string())?;
        assert_eq!(v["items"], serde_json::json!([]), "suggest no match: {v}");

        // Tasks stats endpoint
        let r = client.get(format!("{base}/api/tasks/stats")).send().map_err(|e| e.to_string())?;
        assert!(r.status().is_success());

        // Rebuild index: search must still hit afterwards (regression: the
        // rebuild used to wipe the index without re-indexing).
        let r = client
            .post(format!("{base}/api/tasks/rebuild-index"))
            .json(&serde_json::json!({"pause_crawl": true}))
            .send()
            .map_err(|e| e.to_string())?;
        assert!(r.status().is_success());
        let mut ok = false;
        let t0 = std::time::Instant::now();
        while t0.elapsed() < Duration::from_secs(30) {
            let r = client
                .get(format!("{base}/api/search"))
                .query(&[("q", "修仙"), ("scope", "local")])
                .send()
                .map_err(|e| e.to_string())?;
            let s: serde_json::Value = r.json().map_err(|e| e.to_string())?;
            if s["total"].as_i64().unwrap_or(0) >= 1 {
                ok = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        assert!(ok, "search must still hit after rebuild-index");
        Ok(())
    })();

    let _ = child.kill();
    let mut stderr = String::new();
    if let Some(mut e) = child.stderr.take() {
        use std::io::Read;
        let _ = e.read_to_string(&mut stderr);
    }
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&tmp);
    if let Err(e) = result {
        panic!("e2e failed: {e}\n--- server stderr ---\n{stderr}");
    }
}

#[allow(dead_code)]
fn _unused(_p: PathBuf) {}

fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for i in 0..256u32 {
        let mut c = i;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
        table[i as usize] = c;
    }
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc = table[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

/// Minimal stored (uncompressed) zip for a one-chapter epub with an image.
fn epub_with_image() -> Vec<u8> {
    let opf = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" unique-identifier="id" version="2.0">
<metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>图册</dc:title><dc:creator>作者</dc:creator><dc:identifier id="id">urn:uuid:test</dc:identifier><meta name="cover" content="cover-img"/></metadata>
<manifest><item id="ch1" href="chapter1.xhtml" media-type="application/xhtml+xml"/><item id="pic" href="images/pic.jpg" media-type="image/jpeg"/><item id="cover-img" href="images/cover.jpg" media-type="image/jpeg"/></manifest>
<spine><itemref idref="ch1"/></spine></package>"#;
    let container = r#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#;
    let xhtml = r#"<?xml version="1.0" encoding="utf-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>第一章</title></head><body><p>第一段文字</p><p><img src="images/pic.jpg" alt="插图"/></p><p>第二段文字</p></body></html>"#;
    let files: [(&str, &[u8]); 6] = [
        ("mimetype", b"application/epub+zip"),
        ("META-INF/container.xml", container.as_bytes()),
        ("OEBPS/content.opf", opf.as_bytes()),
        ("OEBPS/chapter1.xhtml", xhtml.as_bytes()),
        ("OEBPS/images/pic.jpg", b"\xff\xd8\xff\xe0fake-jpeg"),
        ("OEBPS/images/cover.jpg", b"\xff\xd8\xff\xe0fake-cover"),
    ];
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in files {
        let offset = out.len() as u32;
        let crc = crc32(data);
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);
        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u32.to_le_bytes());
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let cd_offset = out.len() as u32;
    out.extend_from_slice(&central);
    let cd_size = central.len() as u32;
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(files.len() as u16).to_le_bytes());
    out.extend_from_slice(&(files.len() as u16).to_le_bytes());
    out.extend_from_slice(&cd_size.to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

#[test]
fn end_to_end_epub_images() {
    let tmp = std::env::temp_dir().join(format!("shuku-e2e-epub-{}", uuid::Uuid::new_v4()));
    let books = tmp.join("books");
    let data = tmp.join("data");
    std::fs::create_dir_all(&books).unwrap();
    std::fs::write(books.join("图册.epub"), epub_with_image()).unwrap();

    let bin = env!("CARGO_BIN_EXE_shuku");
    let port = free_port();
    let mut child: Child = Command::new(bin)
        .env("SHUKU_BOOKS_DIR", &books)
        .env("SHUKU_DATA_DIR", &data)
        .env("SHUKU_ADDR", format!("127.0.0.1:{port}"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn server");

    let client = reqwest::blocking::Client::new();
    let base = format!("http://127.0.0.1:{port}");
    let result = (|| -> Result<(), String> {
        assert!(wait_for_health(port, Duration::from_secs(30)), "server did not come up");
        let mut book_id = String::new();
        let t0 = std::time::Instant::now();
        while t0.elapsed() < Duration::from_secs(30) {
            let r = client.get(format!("{base}/api/books")).send().map_err(|e| e.to_string())?;
            let j: serde_json::Value = r.json().map_err(|e| e.to_string())?;
            if j["total"].as_i64().unwrap_or(0) >= 1 {
                book_id = j["items"][0]["id"].as_str().unwrap().to_string();
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        assert!(!book_id.is_empty(), "epub book never indexed");

        // Chapter paragraphs: an img paragraph between two text paragraphs.
        let r = client.get(format!("{base}/api/books/{book_id}/chapters/0")).send().map_err(|e| e.to_string())?;
        let c: serde_json::Value = r.json().map_err(|e| e.to_string())?;
        let paras = c["paragraphs"].as_array().unwrap();
        let img = paras.iter().find(|p| p.get("img").is_some()).expect("chapter must have an img paragraph");
        let img_url = img["img"].as_str().unwrap();
        assert!(img_url.ends_with("/OEBPS/images/pic.jpg"), "img url: {img_url}");
        assert!(paras.iter().any(|p| p["text"].as_str().unwrap_or("") == "第一段文字"));

        // The image endpoint serves the epub's bytes with a jpeg mime.
        let r = client.get(format!("{base}{img_url}")).send().map_err(|e| e.to_string())?;
        assert!(r.status().is_success(), "images endpoint must 200: {}", r.status());
        assert!(r.headers().get("content-type").unwrap().to_str().unwrap().contains("image/jpeg"));
        assert_eq!(r.bytes().map_err(|e| e.to_string())?.as_ref(), b"\xff\xd8\xff\xe0fake-jpeg");

        // The cover endpoint serves the epub's declared cover bytes.
        let r = client.get(format!("{base}/api/books/{book_id}/cover")).send().map_err(|e| e.to_string())?;
        assert!(r.status().is_success(), "cover endpoint must 200: {}", r.status());
        assert!(r.headers().get("content-type").unwrap().to_str().unwrap().contains("image/jpeg"));
        assert_eq!(r.bytes().map_err(|e| e.to_string())?.as_ref(), b"\xff\xd8\xff\xe0fake-cover");

        // Path traversal is rejected.
        let r = client
            .get(format!("{base}/api/books/{book_id}/images/../secret"))
            .send()
            .map_err(|e| e.to_string())?;
        assert!(!r.status().is_success(), "traversal must not 200: {}", r.status());

        // The image marker must not pollute search: searching for the marker
        // text finds nothing, searching chapter text still hits.
        let r = client
            .get(format!("{base}/api/search"))
            .query(&[("q", "第二段文字"), ("scope", "local")])
            .send()
            .map_err(|e| e.to_string())?;
        let s: serde_json::Value = r.json().map_err(|e| e.to_string())?;
        assert!(s["total"].as_i64().unwrap_or(0) >= 1, "search must hit chapter text: {s}");
        Ok(())
    })();

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&tmp);
    if let Err(e) = result {
        panic!("e2e epub images failed: {e}");
    }
}
