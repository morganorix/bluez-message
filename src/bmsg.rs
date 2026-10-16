//! Encodeur bMessage (MAP) pour `SMS_GSM`.

use crate::error::{Error, Result};

/// Numéro : `+?` suivi de 3 à 20 chiffres.
///
/// # Errors
/// [`Error::InvalidNumber`] si le numéro n'a pas cette forme.
pub fn validate_number(n: &str) -> Result<&str> {
    let digits = n.strip_prefix('+').unwrap_or(n);
    if (3..=20).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit()) {
        Ok(n)
    } else {
        Err(Error::InvalidNumber(n.to_owned()))
    }
}

/// Neutralise les séquences structurelles du format : le corps ne doit pas
/// pouvoir contenir une ligne `END:MSG`/`END:BBODY`, ni de CR/LF libres.
fn sanitize_body(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .split('\n')
        .map(|l| {
            if l.trim_start().to_ascii_uppercase().starts_with("END:")
                || l.trim_start().to_ascii_uppercase().starts_with("BEGIN:")
            {
                format!(" {l}")
            } else {
                l.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\r\n")
}

/// bMessage SMS (vCard du destinataire, corps UTF-8) pour `PushMessage`.
///
/// # Errors
/// Numéro invalide (voir [`validate_number`]).
pub fn encode_sms(to: &str, text: &str) -> Result<String> {
    let to = validate_number(to)?;
    let body = format!("BEGIN:MSG\r\n{}\r\nEND:MSG\r\n", sanitize_body(text));
    Ok(format!(
        "BEGIN:BMSG\r\nVERSION:1.0\r\nSTATUS:UNREAD\r\nTYPE:SMS_GSM\r\nFOLDER:telecom/msg/outbox\r\n\
         BEGIN:VCARD\r\nVERSION:2.1\r\nN:\r\nTEL:{to}\r\nEND:VCARD\r\n\
         BEGIN:BENV\r\nBEGIN:VCARD\r\nVERSION:2.1\r\nN:\r\nTEL:{to}\r\nEND:VCARD\r\n\
         BEGIN:BBODY\r\nCHARSET:UTF-8\r\nLENGTH:{}\r\n{body}END:BBODY\r\nEND:BENV\r\nEND:BMSG\r\n",
        body.len()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn length_is_utf8_bytes() {
        let m = encode_sms("+33712345601", "é").unwrap();
        let body = "BEGIN:MSG\r\né\r\nEND:MSG\r\n";
        assert!(m.contains(&format!("LENGTH:{}\r\n", body.len())));
        assert_eq!(body.len(), 24);
    }

    #[test]
    fn injection_is_neutralised() {
        let m = encode_sms("+33712345601", "a\r\nEND:MSG\r\nEND:BBODY").unwrap();
        assert_eq!(m.matches("END:MSG").count(), 2); // 1 réel + 1 neutralisé (indenté)
        assert!(m.contains("\r\n END:MSG\r\n"));
        assert!(m.contains("\r\n END:BBODY\r\n"));
    }

    #[test]
    fn bad_numbers_rejected() {
        for n in ["", "+", "12", "+33 7", "abc", "+3371234560112345678901", "+33\r\nTEL:1"] {
            assert!(validate_number(n).is_err(), "{n:?}");
        }
        assert!(validate_number("0712345601").is_ok());
    }
}
