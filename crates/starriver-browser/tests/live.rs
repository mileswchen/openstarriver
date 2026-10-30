//! Live-browser integration tests: exercise the real CDP path against a
//! locally-installed Chrome. Ignored by default — run explicitly with
//! `cargo test -p starriver-browser --test live -- --ignored` on a machine
//! that has Chrome/Chromium/Edge (or set `browser_path` via StartOptions).

use std::sync::Arc;

use starriver_browser::{BrowserService, BrowserServiceConfig, StartOptions};

fn live_config() -> BrowserServiceConfig {
    let base = std::env::var("STARRIVER_TEST_TMP")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    let mut config = BrowserServiceConfig::default();
    config.profile_root = base.join(format!("starriver-browser-live-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&config.profile_root);
    config
}

/// Minimal HTTP server serving one page; returns its base URL.
async fn serve_page(body: &'static str) -> String {
    use tokio::io::AsyncWriteExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let body = body;
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buf).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    format!("http://{addr}/")
}

const PAGE: &str = r#"<!doctype html>
<html><head><title>Live Test</title></head><body>
<h1>Form</h1>
<form onsubmit="event.preventDefault(); document.getElementById('out').textContent =
  'submitted:' + document.getElementById('name').value;">
  <input id="name" type="text" placeholder="Name">
  <button id="go" type="submit">Go</button>
</form>
<div id="out">initial</div>
<script>document.title = 'Live Test'</script>
</body></html>"#;

#[tokio::test]
#[ignore = "needs a locally installed Chrome/Chromium/Edge"]
async fn live_session_full_flow() {
    let service = Arc::new(BrowserService::new(live_config()));
    let info = service
        .start(StartOptions {
            owner: Some("live-test".into()),
            ..Default::default()
        })
        .await
        .expect("browser start (is Chrome installed?)");
    let id = info.id;

    // eval works on the initial about:blank page.
    let two = service.eval(&id, "1 + 1", 100).await.unwrap();
    assert_eq!(two, "2", "{two}");

    // navigate to a local page, settle, snapshot.
    let base = serve_page(PAGE).await;
    let nav = service.navigate(&id, &base, 15_000).await.unwrap();
    assert_eq!(nav.title, "Live Test", "{}", nav.final_url);

    let snapshot = service.snapshot(&id, 20_000).await.unwrap();
    assert!(snapshot.contains("url: "), "{snapshot}");
    assert!(snapshot.contains("heading 1 \"Form\""), "{snapshot}");
    assert!(snapshot.contains("[e"), "no refs in snapshot: {snapshot}");
    let textbox_ref = snapshot
        .lines()
        .find(|l| l.contains("textbox") && l.contains("Name"))
        .and_then(|l| l.split('[').nth(1))
        .and_then(|r| r.split(']').next())
        .expect("textbox ref")
        .to_string();
    let button_ref = snapshot
        .lines()
        .find(|l| l.contains("button") && l.contains("Go"))
        .and_then(|l| l.split('[').nth(1))
        .and_then(|r| r.split(']').next())
        .expect("button ref")
        .to_string();

    // type + submit via the form (submit path).
    service
        .type_text(&id, &textbox_ref, "ada", true)
        .await
        .unwrap();
    let out = service
        .eval(&id, "document.getElementById('out').textContent", 200)
        .await
        .unwrap();
    assert_eq!(out, "\"submitted:ada\"", "{out}");

    // click path: reset, then click the button directly.
    service
        .eval(
            &id,
            "document.getElementById('out').textContent = 'reset'",
            200,
        )
        .await
        .unwrap();
    service.click(&id, &button_ref).await.unwrap();
    let out = service
        .eval(&id, "document.getElementById('out').textContent", 200)
        .await
        .unwrap();
    assert_eq!(out, "\"submitted:ada\"", "{out}");

    // stale refs after a re-navigation error with the re-snapshot hint.
    service.navigate(&id, &base, 15_000).await.unwrap();
    let err = service.click(&id, &button_ref).await.unwrap_err();
    assert_eq!(err.code(), "STALE_REF", "{err}");

    // screenshot produces a PNG.
    let png = service.screenshot(&id, false).await.unwrap();
    assert_eq!(&png[..4], b"\x89PNG", "not a PNG");

    // list + close: session leaves the registry and the profile dir.
    let listed = service.list();
    assert!(listed.iter().any(|s| s.id == id), "{listed:?}");
    service.close(&id).await.unwrap();
    assert!(service.list().iter().all(|s| s.id != id));
}
