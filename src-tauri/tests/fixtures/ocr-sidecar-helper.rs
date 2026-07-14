use std::io::{self, BufRead, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

fn main() {
    if std::env::args().nth(1).as_deref() == Some("--descendant") {
        std::thread::sleep(Duration::from_secs(3));
        return;
    }

    let executable = std::env::current_exe().unwrap();
    if executable
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.contains("no-stdin"))
    {
        let stdin = io::stdin();
        let mut lines = stdin.lock().lines();
        let request = lines.next().unwrap().unwrap();
        assert!(request.contains("arm-no-stdin.pdf"));
        println!(r#"{{"ok":true,"text":"armed","warnings":[]}}"#);
        io::stdout().flush().unwrap();
        let descendant = Command::new(&executable)
            .arg("--descendant")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        std::fs::write(executable.with_extension("pid"), descendant.id().to_string()).unwrap();
        std::thread::sleep(Duration::from_secs(3));
        return;
    }

    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let request = line.unwrap();
        assert!(request.contains("\"path\""));
        handle_request(&request);
        io::stdout().flush().unwrap();
    }
}

fn handle_request(request: &str) {
    if request.contains(r#""operation":"extract_pdf_text""#) {
        if request.contains("compressed-text-bomb.pdf") {
            println!(
                r#"{{"ok":false,"error":"document exceeds OCR resource limits"}}"#
            );
        } else if request.contains("malformed-content.pdf") {
            println!(r#"{{"ok":false,"error":"Unable to extract PDF text."}}"#);
        } else if request.contains("text-invoice.pdf") {
            println!(
                r#"{{"ok":true,"text":"开票日期：2026年06月18日\n价税合计（小写）¥128.50","warnings":[]}}"#
            );
        } else {
            println!(r#"{{"ok":true,"text":"","warnings":[]}}"#);
        }
        return;
    }

    if request.contains("orphan-pipe") {
        let descendant = Command::new(std::env::current_exe().unwrap())
            .arg("--descendant")
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        std::fs::write(request_path(request), descendant.id().to_string()).unwrap();
        std::process::exit(0);
    } else if request.contains("descendant-timeout") {
        let descendant = Command::new(std::env::current_exe().unwrap())
            .arg("--descendant")
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        std::fs::write(request_path(request), descendant.id().to_string()).unwrap();
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
    } else if request.contains("persistent-session.pdf") {
        println!(
            r#"{{"ok":true,"text":"{}","warnings":[]}}"#,
            std::process::id()
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
