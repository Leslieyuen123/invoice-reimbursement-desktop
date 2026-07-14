use std::io::{self, Read};
use std::process::{Command, Stdio};
use std::time::Duration;

fn main() {
    if std::env::args().nth(1).as_deref() == Some("--descendant") {
        std::thread::sleep(Duration::from_secs(3));
        return;
    }

    let mut request = String::new();
    io::stdin().read_to_string(&mut request).unwrap();
    assert!(request.ends_with('\n'));
    assert!(request.contains("\"path\""));

    if request.contains("descendant-timeout") {
        let descendant = Command::new(std::env::current_exe().unwrap())
            .arg("--descendant")
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        std::fs::write(request_path(&request), descendant.id().to_string()).unwrap();
        std::thread::sleep(Duration::from_secs(3));
    } else if request.contains("timeout.pdf") {
        std::thread::sleep(Duration::from_secs(3));
        println!(r#"{{"ok":true,"text":"too late","warnings":[]}}"#);
    } else if request.contains("malformed.pdf") {
        println!("not-json super-secret-sidecar-detail");
    } else if request.contains("crash.pdf") {
        eprintln!("stderr-secret /private/internal/location");
        std::process::exit(7);
    } else if request.contains("customer-secret-error.pdf") {
        println!(
            r#"{{"ok":false,"error":"super-secret-sidecar-detail /private/path"}}"#
        );
    } else if request.contains("warning-sanitize.pdf") {
        let mut warnings = vec![
            "low_confidence".to_owned(),
            "/private/super-secret".to_owned(),
            "x".repeat(200),
        ];
        warnings.extend((0..20).map(|index| format!("warning_{index}")));
        let warnings = warnings
            .into_iter()
            .map(|warning| format!(r#""{warning}""#))
            .collect::<Vec<_>>()
            .join(",");
        println!(
            r#"{{"ok":true,"text":"warning text","warnings":[{warnings}]}}"#
        );
    } else {
        println!(
            r#"{{"ok":true,"text":"北京 出租车 价税合计 ¥128.50","warnings":["low_confidence"]}}"#
        );
    }
}

fn request_path(request: &str) -> &str {
    let marker = r#""path":""#;
    let start = request.find(marker).unwrap() + marker.len();
    let remaining = &request[start..];
    let end = remaining.find('"').unwrap();
    &remaining[..end]
}
