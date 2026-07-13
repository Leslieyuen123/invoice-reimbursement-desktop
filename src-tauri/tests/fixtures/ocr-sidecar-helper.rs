use std::io::{self, Read};
use std::time::Duration;

fn main() {
    let mut request = String::new();
    io::stdin().read_to_string(&mut request).unwrap();
    assert!(request.ends_with('\n'));
    assert!(request.contains("\"path\""));

    if request.contains("timeout.pdf") {
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
    } else {
        println!(
            r#"{{"ok":true,"text":"北京 出租车 价税合计 ¥128.50","warnings":["low_confidence"]}}"#
        );
    }
}
