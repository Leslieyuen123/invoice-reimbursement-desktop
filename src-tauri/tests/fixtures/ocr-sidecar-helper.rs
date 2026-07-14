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
        let descendant = Command::new(&executable)
            .arg("--descendant")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        println!(
            r#"{{"ok":true,"text":"{}","warnings":[]}}"#,
            descendant.id()
        );
        io::stdout().flush().unwrap();
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
    if request.contains("hold-session-lock.pdf") {
        #[cfg(unix)]
        {
            use std::io::Read as _;
            use std::os::unix::net::UnixStream;

            let ready_socket = std::env::current_exe().unwrap().with_extension("ready.sock");
            let mut ready = UnixStream::connect(ready_socket).unwrap();
            ready.write_all(b"ready").unwrap();
            let mut release = [0_u8];
            let _ = ready.read_exact(&mut release);
        }
        println!(r#"{{"ok":true,"text":"lock released","warnings":[]}}"#);
        return;
    }

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

    if request.contains("arm-descendant.pdf") {
        let descendant = Command::new(std::env::current_exe().unwrap())
            .arg("--descendant")
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        println!(
            r#"{{"ok":true,"text":"{}","warnings":[]}}"#,
            descendant.id()
        );
    } else if request.contains("trigger-orphan-pipe.pdf") {
        std::process::exit(0);
    } else if request.contains("trigger-descendant-timeout.pdf") {
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
