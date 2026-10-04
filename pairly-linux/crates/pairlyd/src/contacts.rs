//! Contacts fetched from a phone are also written as a vCard file
//! (`<data dir>/contacts/<device>.vcf`), for address books and other tools.

use std::path::Path;

use pairly_core::DeviceId;
use pairly_plugins::contacts::Contact;
use tracing::debug;

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace(',', "\\,")
        .replace(';', "\\;")
        .replace('\n', "\\n")
}

pub fn vcards(contacts: &[Contact]) -> String {
    let mut out = String::new();
    for c in contacts {
        out.push_str("BEGIN:VCARD\r\nVERSION:3.0\r\n");
        out.push_str(&format!("FN:{}\r\n", escape(&c.name)));
        for n in &c.numbers {
            out.push_str(&format!("TEL;TYPE=CELL:{}\r\n", escape(n)));
        }
        out.push_str("END:VCARD\r\n");
    }
    out
}

pub fn save_vcards(data_dir: &Path, peer: DeviceId, contacts: &[Contact]) {
    let dir = data_dir.join("contacts");
    let result = std::fs::create_dir_all(&dir)
        .and_then(|()| std::fs::write(dir.join(format!("{peer}.vcf")), vcards(contacts)));
    if let Err(e) = result {
        debug!(error = %e, "can't save contacts");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vcard_format() {
        let v = vcards(&[Contact {
            name: "Doe, Jane".into(),
            numbers: vec!["+1 555".into()],
        }]);
        assert!(v.contains("FN:Doe\\, Jane\r\n"));
        assert!(v.contains("TEL;TYPE=CELL:+1 555\r\n"));
    }
}
