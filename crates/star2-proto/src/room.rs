const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";
const SECRET_LEN: usize = 10;
const MAX_LABEL: usize = 24;

pub fn slugify(name: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in name.chars() {
        if out.len() >= MAX_LABEL {
            break;
        }
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            dash = false;
        } else if !out.is_empty() && !dash {
            out.push('-');
            dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "room".to_string()
    } else {
        out
    }
}

pub fn new_room_token(name: &str) -> String {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).expect("os rng");
    let mut bits = u64::from_le_bytes(b);
    let mut secret = String::with_capacity(SECRET_LEN);
    for _ in 0..SECRET_LEN {
        secret.push(ALPHABET[(bits & 31) as usize] as char);
        bits >>= 5;
    }
    format!("{}-{}", slugify(name), secret)
}

pub fn room_label(token: &str) -> &str {
    match token.rsplit_once('-') {
        Some((label, secret)) if !label.is_empty() && is_secret(secret) => label,
        _ => token,
    }
}

pub fn is_room_token(s: &str) -> bool {
    matches!(s.rsplit_once('-'), Some((l, sec)) if !l.is_empty() && is_secret(sec))
}

fn is_secret(s: &str) -> bool {
    s.len() == SECRET_LEN && s.bytes().all(|b| ALPHABET.contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_cases() {
        assert_eq!(slugify("gaming"), "gaming");
        assert_eq!(slugify("Friday Night"), "friday-night");
        assert_eq!(slugify("  a//b  "), "a-b");
        assert_eq!(slugify("!!!"), "room");
        assert_eq!(slugify(""), "room");
        assert!(slugify(&"x".repeat(100)).len() <= MAX_LABEL);
    }

    #[test]
    fn token_label_roundtrip() {
        let t = new_room_token("Friday Night");
        assert_eq!(room_label(&t), "friday-night");
        assert!(is_room_token(&t));
        assert_eq!(t.len(), "friday-night".len() + 1 + SECRET_LEN);
    }

    #[test]
    fn token_secret_is_from_alphabet() {
        let t = new_room_token("x");
        let (_, sec) = t.rsplit_once('-').unwrap();
        assert_eq!(sec.len(), SECRET_LEN);
        assert!(sec.bytes().all(|b| ALPHABET.contains(&b)));
    }

    #[test]
    fn tokens_differ() {
        let a = new_room_token("same");
        let b = new_room_token("same");
        assert_ne!(a, b);
    }

    #[test]
    fn plain_names_are_not_tokens() {
        assert!(!is_room_token("general"));
        assert!(!is_room_token("my-room"));
        assert!(!is_room_token(""));
        assert_eq!(room_label("general"), "general");
    }

    #[test]
    fn ambiguous_chars_excluded() {
        for c in [b'i', b'l', b'o', b'u'] {
            assert!(!ALPHABET.contains(&c));
        }
    }
}
